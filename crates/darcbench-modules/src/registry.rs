//! The module allow-list.
//!
//! # Security role
//!
//! This registry is the boundary that makes a browser-driven benchmark safe.
//! An API caller supplies a [`ModuleId`] *string*; the registry either maps it
//! to a compiled-in implementation or rejects it. There is no path from a
//! request body to a process, a path, or a shell.
//!
//! Consequently `Registry::get` must never fall back to "try to interpret this
//! as something executable". Unknown means rejected. See
//! `docs/THREAT-MODEL.md` (T-AGENT-RCE).

use std::collections::BTreeMap;
use std::sync::Arc;

use darcbench_protocol::{ModuleId, ModuleRef, Profile};

use crate::cpu_mixed::CpuMixed;
use crate::memory_bandwidth::MemoryBandwidth;
use crate::module::BenchmarkModule;
use crate::network_transfer::NetworkTransfer;
use crate::storage_mixed::StorageMixed;

/// Immutable set of modules this agent build can run.
#[derive(Clone)]
pub struct Registry {
    modules: BTreeMap<ModuleId, Arc<dyn BenchmarkModule>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("modules", &self.ids())
            .finish()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::builtin()
    }
}

impl Registry {
    /// Every first-party module compiled into this build.
    pub fn builtin() -> Self {
        let mut modules: BTreeMap<ModuleId, Arc<dyn BenchmarkModule>> = BTreeMap::new();
        let cpu_mixed = Arc::new(CpuMixed::new());
        modules.insert(cpu_mixed.manifest().id.clone(), cpu_mixed);
        let memory_bandwidth = Arc::new(MemoryBandwidth::new());
        modules.insert(memory_bandwidth.manifest().id.clone(), memory_bandwidth);
        let storage_mixed = Arc::new(StorageMixed::new());
        modules.insert(storage_mixed.manifest().id.clone(), storage_mixed);
        let network_transfer = Arc::new(NetworkTransfer::new());
        modules.insert(network_transfer.manifest().id.clone(), network_transfer);
        let web_static: Arc<dyn BenchmarkModule> = Arc::new(crate::web_static::WebStatic::new());
        modules.insert(web_static.manifest().id.clone(), web_static);
        let php_runtime: Arc<dyn BenchmarkModule> = Arc::new(crate::php_runtime::PhpRuntime::new());
        modules.insert(php_runtime.manifest().id.clone(), php_runtime);
        let node_runtime: Arc<dyn BenchmarkModule> =
            Arc::new(crate::node_runtime::NodeRuntime::new());
        modules.insert(node_runtime.manifest().id.clone(), node_runtime);
        let database_oltp: Arc<dyn BenchmarkModule> =
            Arc::new(crate::database_oltp::DatabaseOltp::new());
        modules.insert(database_oltp.manifest().id.clone(), database_oltp);
        let database_cache: Arc<dyn BenchmarkModule> =
            Arc::new(crate::database_cache::DatabaseCache::new());
        modules.insert(database_cache.manifest().id.clone(), database_cache);
        Self { modules }
    }

    pub fn get(&self, id: &ModuleId) -> Option<Arc<dyn BenchmarkModule>> {
        self.modules.get(id).cloned()
    }

    pub fn ids(&self) -> Vec<ModuleId> {
        self.modules.keys().cloned().collect()
    }

    pub fn refs(&self) -> Vec<ModuleRef> {
        self.modules
            .values()
            .map(|m| m.manifest().module_ref())
            .collect()
    }

    pub fn manifests(&self) -> Vec<crate::ModuleManifest> {
        self.modules
            .values()
            .map(|m| m.manifest().clone())
            .collect()
    }

    /// Modules a profile runs, in execution order.
    ///
    /// Only implemented modules appear: a profile resolves to what this build
    /// can actually run, never to a wish list. The web-only profile is still
    /// empty rather than pretending to run workloads that do not exist, and
    /// `docs/ROADMAP.md` tracks when each profile becomes complete.
    ///
    /// `web.static` runs last in every profile that includes it. It is the only
    /// module that starts a listener, and putting it after the others means a
    /// machine where binding loopback fails - a locked-down container, an
    /// exotic sandbox - has still produced every other result by the time that
    /// shows up.
    ///
    /// Order matters, cheapest and least invasive first: `cpu.mixed` allocates
    /// almost nothing, `memory.bandwidth` allocates but writes nothing, and
    /// `storage.mixed` writes. A machine too constrained for a later module has
    /// still produced every earlier result by the time that shows up.
    ///
    /// `ReadOnly` deliberately omits `storage.mixed`: it is the profile for a
    /// machine the operator will not write to. `Profile::is_standard` returns
    /// false for it, so the resulting run is `Custom` and cannot be mistaken
    /// for a full storage measurement.
    ///
    /// `Quick` deliberately omits `network.transfer`, and so does `ReadOnly`.
    /// The quick profile is the first thing anyone runs on a machine they are
    /// evaluating, and keeping it free of egress means that first run contacts
    /// nothing and consumes none of the host's bandwidth - which may be
    /// metered, and is certainly not free on a machine already serving traffic.
    /// A profile that claims a comparable total needs the Network category and
    /// therefore accepts the egress, disclosed by preflight before it starts.
    pub fn modules_for_profile(&self, profile: Profile) -> Vec<ModuleId> {
        let has = |name: &str| -> Option<ModuleId> {
            ModuleId::new(name)
                .ok()
                .filter(|id| self.modules.contains_key(id))
        };
        match profile {
            // The web profile is now what its name says: the web module and
            // nothing else. It stays `is_standard`, so a run of it is
            // rankable - but with only one category measured it can never
            // produce a *standard total*, and `missing_required_categories`
            // says which four are absent.
            Profile::WebOnly => [has("web.static"), has("php.runtime"), has("node.runtime")]
                .into_iter()
                .flatten()
                .collect(),
            Profile::ReadOnly => [has("cpu.mixed"), has("memory.bandwidth")]
                .into_iter()
                .flatten()
                .collect(),
            Profile::Quick => [
                has("cpu.mixed"),
                has("memory.bandwidth"),
                has("storage.mixed"),
            ]
            .into_iter()
            .flatten()
            .collect(),
            // Endurance repeats its module set for an hour, which is exactly why
            // `network.transfer` is not in it.
            //
            // That module's transfer ceiling is a bound on what DARCBench will
            // pull from a third party *per run*, and it is the mechanism that
            // keeps the suite from being a traffic amplifier. Cycling the module
            // fifteen times would either breach that bound fifteen-fold or
            // divide each cycle's transfer until the measurement said nothing -
            // and the honest version of the second option is not to run it.
            // Sustained load on somebody else's CDN is not ours to generate.
            //
            // A real gap comes with that: bandwidth quotas and traffic shaping
            // are exactly the kind of thing an hour-long run should catch. It
            // needs an endpoint whose operator has agreed to an hour of
            // traffic, which is a conversation rather than a code change, and
            // it is on the backlog as one.
            Profile::Endurance => [
                has("cpu.mixed"),
                has("memory.bandwidth"),
                has("storage.mixed"),
            ]
            .into_iter()
            .flatten()
            .collect(),
            // `php.runtime` is in `deep` and `web` but deliberately **not** in
            // `standard`. It executes an interpreter the operator installed, and
            // most machines have no PHP at all - a standard run that came back
            // `Partial` on every machine that is not a PHP host would be
            // reporting the profile's own assumptions as a fault of the
            // machine. `deep` and `web` are chosen by an operator who wants
            // them. See docs/adr/0013-executing-a-discovered-runtime.md.
            Profile::Standard | Profile::Custom => [
                has("cpu.mixed"),
                has("memory.bandwidth"),
                has("storage.mixed"),
                has("network.transfer"),
                has("web.static"),
            ]
            .into_iter()
            .flatten()
            .collect(),
            Profile::Deep => [
                has("cpu.mixed"),
                has("memory.bandwidth"),
                has("storage.mixed"),
                has("network.transfer"),
                has("web.static"),
                has("php.runtime"),
                has("node.runtime"),
                // The database modules are in `deep` alone, and the argument is
                // `php.runtime`'s exactly: they need a container runtime with a
                // reachable daemon, most machines in this market have neither,
                // and a standard run returning `Partial` on every one of them
                // would be reporting the profile's assumptions as a fault of
                // the machine.
                //
                // They also run last. Between them they start two containers,
                // pull nothing but hold a gigabyte each while running, and are
                // the only modules whose failure depends on a daemon rather
                // than on this process - so a machine that cannot do it has
                // still produced every other result by the time that shows up.
                // Same ordering rule as `web.static`, one tier further out.
                has("database.oltp"),
                has("database.cache"),
            ]
            .into_iter()
            .flatten()
            .collect(),
        }
    }

    /// Validates a caller-supplied module list against the allow-list.
    ///
    /// Returns the unknown ids so the API can report precisely what was
    /// rejected instead of failing opaquely.
    pub fn validate(&self, requested: &[ModuleId]) -> Result<(), Vec<ModuleId>> {
        let unknown: Vec<ModuleId> = requested
            .iter()
            .filter(|id| !self.modules.contains_key(*id))
            .cloned()
            .collect();
        if unknown.is_empty() {
            Ok(())
        } else {
            Err(unknown)
        }
    }
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    /// Every module this build ships must be reachable by its declared id.
    #[test]
    fn every_builtin_module_resolves_by_id() {
        let registry = Registry::builtin();
        for name in [
            "cpu.mixed",
            "memory.bandwidth",
            "storage.mixed",
            "network.transfer",
        ] {
            let id = ModuleId::new(name).expect("id");
            assert!(registry.get(&id).is_some(), "`{name}` is not registered");
        }
        assert_eq!(
            registry.ids().len(),
            registry.manifests().len(),
            "every registered id must have a manifest"
        );
    }

    /// A registered module must declare the id it is registered under.
    ///
    /// The registry keys on `manifest().id`, so a copy-paste that left a new
    /// module claiming an existing id would silently replace it rather than
    /// being added.
    #[test]
    fn no_two_modules_share_an_id() {
        let registry = Registry::builtin();
        let ids = registry.ids();
        let unique: std::collections::BTreeSet<&ModuleId> = ids.iter().collect();
        assert_eq!(
            ids.len(),
            unique.len(),
            "duplicate module id in the registry"
        );
        for manifest in registry.manifests() {
            assert!(ids.contains(&manifest.id));
        }
    }

    #[test]
    fn unknown_modules_are_rejected_not_interpreted() {
        let registry = Registry::builtin();
        for candidate in ["storage.fio", "cpu.mixed2", "custom_thing"] {
            let Ok(id) = ModuleId::new(candidate) else {
                continue;
            };
            assert!(
                registry.get(&id).is_none(),
                "`{candidate}` must not resolve"
            );
            assert!(registry.validate(&[id]).is_err());
        }
    }

    #[test]
    fn validate_reports_every_unknown_id() {
        let registry = Registry::builtin();
        let known = ModuleId::new("cpu.mixed").expect("id");
        let unknown_a = ModuleId::new("nope.one").expect("id");
        let unknown_b = ModuleId::new("nope.two").expect("id");
        let err = registry
            .validate(&[known, unknown_a.clone(), unknown_b.clone()])
            .expect_err("should reject");
        assert_eq!(err, vec![unknown_a, unknown_b]);
    }

    #[test]
    fn profile_resolution_never_invents_modules() {
        let registry = Registry::builtin();
        for profile in [
            Profile::Quick,
            Profile::Standard,
            Profile::Deep,
            Profile::Endurance,
            Profile::ReadOnly,
            Profile::WebOnly,
            Profile::Custom,
        ] {
            for id in registry.modules_for_profile(profile) {
                assert!(
                    registry.get(&id).is_some(),
                    "{profile} resolved to unimplemented module {id}"
                );
            }
        }
        // The web profile runs the web module and nothing else.
        assert_eq!(
            registry.modules_for_profile(Profile::WebOnly),
            vec![
                ModuleId::new("web.static").expect("id"),
                ModuleId::new("php.runtime").expect("id"),
                ModuleId::new("node.runtime").expect("id"),
            ]
        );
        // `php.runtime` executes the operator's interpreter and most machines
        // have none, so it must never be in the profile that claims a standard
        // total.
        for runtime in ["php.runtime", "node.runtime"] {
            assert!(
                !registry
                    .modules_for_profile(Profile::Standard)
                    .iter()
                    .any(|id| id.as_str() == runtime),
                "`{runtime}` executes a binary the operator installed and must stay out of the \
                 profile that claims a standard total"
            );
            assert!(registry
                .modules_for_profile(Profile::Deep)
                .iter()
                .any(|id| id.as_str() == runtime));
        }

        // `cpu.mixed` runs first: it allocates almost nothing, so a machine too
        // constrained for a credible memory working set has still produced its
        // compute result by the time that shows up.
        let quick = registry.modules_for_profile(Profile::Quick);
        assert_eq!(
            quick.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec!["cpu.mixed", "memory.bandwidth", "storage.mixed"]
        );

        // The read-only profile must not run anything that writes, and must
        // not be able to claim a standard total for the storage it never
        // measured.
        let read_only = registry.modules_for_profile(Profile::ReadOnly);
        assert!(
            !read_only.iter().any(|id| id.as_str() == "storage.mixed"),
            "the read-only profile must not run a module that writes"
        );
        for id in &read_only {
            let module = registry.get(id).expect("registered");
            assert_eq!(
                module.manifest().max_bytes_written,
                0,
                "{id} writes, so it does not belong in the read-only profile"
            );
        }
        assert!(!Profile::ReadOnly.is_standard());

        // The first run anyone makes on a machine must not contact the
        // internet. Only a profile that claims a comparable total accepts the
        // egress, and preflight discloses it before it starts.
        for quiet in [Profile::Quick, Profile::ReadOnly] {
            for id in registry.modules_for_profile(quiet) {
                let module = registry.get(&id).expect("registered");
                assert_eq!(
                    module.manifest().max_network_bytes,
                    0,
                    "{quiet} resolved to {id}, which uses the network"
                );
            }
        }
        assert!(registry
            .modules_for_profile(Profile::Standard)
            .iter()
            .any(|id| id.as_str() == "network.transfer"));
    }

    #[test]
    fn manifests_are_exposed_for_the_api() {
        let manifests = Registry::builtin().manifests();
        assert!(!manifests.is_empty());
        for manifest in &manifests {
            assert!(
                !manifest.limitations.is_empty(),
                "{} ships no limitations; a module that claims none has not thought about \
                 what it measures",
                manifest.id
            );
            assert!(!manifest.validation.is_empty(), "{}", manifest.id);
            assert!(manifest.stability_cv_bound > 0.0, "{}", manifest.id);
        }
    }
}
