//! The WordPress fixture generator: deterministic content for `wordpress.*`.
//!
//! # Why a generator rather than a dump
//!
//! `docs/BENCHMARK-METHODOLOGY.md` is blunt about CMS benchmarks: *"WordPress
//! performance" without a cache disclosure is meaningless.* The same is true of
//! the content. A WordPress serving twelve posts and one serving twelve hundred
//! are different programs at runtime - different query plans, different object
//! cache pressure, different theme loop costs - so a number from one says
//! nothing about the other.
//!
//! Two machines are therefore only comparable if they served **the same
//! content**. That could come from a fixture file shipped beside the binary,
//! and it does not, for three reasons:
//!
//! * A file is a thing that can drift, be edited, or fail to ship.
//! * A file large enough to matter is large enough to bloat a single static
//!   binary that is meant to be downloaded onto a production server.
//! * A generator can be *proved* identical across machines by a checksum, and a
//!   file can only be assumed identical.
//!
//! So the corpus is generated from a fixed seed by the same
//! [`SplitMix64`](crate::workloads::SplitMix64) every other DARCBench corpus
//! uses, and [`Fixture::checksum`] is pinned by a test. Change one word of the
//! generator and that test fails, which is the point: the fixture's content is
//! part of the workload definition, and a workload definition that changes
//! silently makes every historical comparison wrong.
//!
//! # The output is WXR, WordPress's own import format
//!
//! Not SQL, and not a series of `wp post create` calls. SQL binds the fixture
//! to a schema version, and WordPress changes its schema; a thousand CLI
//! invocations would measure process startup a thousand times. WXR is the
//! format WordPress documents, exports and imports, and `wp import` reads it -
//! so the fixture is loaded by the same code path an operator's own migration
//! would use.
//!
//! # Everything generated is inert by construction
//!
//! This content is imported into a CMS and rendered into HTML, so a generator
//! that could emit markup would be writing a stored-XSS payload into every
//! machine that ran the benchmark - and `docs/THREAT-MODEL.md` T-XSS is about
//! exactly that class of thing arriving from a place nobody was watching.
//!
//! The defence is structural rather than filtering: every string in the output
//! is built from [`WORDS`], a fixed list of lowercase ASCII words, and from
//! integers. There is no input to this module at all - no parameter, no
//! configuration, no path - so there is nothing for a caller to inject
//! *through*. On top of that, [`cdata`] splits any `]]>` that could ever appear
//! and [`escape_xml`] handles the attribute and element cases, and both are
//! tested against strings the generator cannot currently produce, precisely so
//! that a future word list cannot quietly make them reachable.

use sha2::{Digest, Sha256};

use crate::workloads::{SplitMix64, CORPUS_SEED};

/// Fixture-definition version. A major bump makes results incomparable.
///
/// Separate from any module's version on purpose: `wordpress.*` can gain a
/// metric without the content changing, and the content can change without the
/// metrics changing, and conflating the two would force a false incomparability
/// in one direction and hide a real one in the other.
pub const FIXTURE_VERSION: &str = "1.0.0";

/// Seed for the fixture corpus.
///
/// Derived from the shared [`CORPUS_SEED`] rather than chosen freshly, so every
/// DARCBench corpus traces back to one constant, and salted so the fixture is
/// not byte-identical to the HTTP origin's bodies - two corpora that happened
/// to share a prefix would be a confusing coincidence to debug.
const FIXTURE_SEED: u64 = CORPUS_SEED ^ 0x5DA7_C0DE;

/// The site the fixture claims to be an export of.
///
/// `example.com` is reserved by RFC 2606 precisely so that it cannot belong to
/// anyone. A generated fixture naming a real domain would put that domain in
/// every imported database and every rendered link on every machine that ran
/// the benchmark.
const SITE_URL: &str = "https://example.com";

/// Vocabulary. Lowercase ASCII words, nothing else.
///
/// The whole of the injection defence, together with the fact that this module
/// takes no input: a corpus assembled from these cannot contain `<`, `&`,
/// `]]>`, a quote or a control character, so nothing it produces can escape the
/// XML it is written into or the HTML it is rendered as.
///
/// Deliberately mundane English rather than lorem ipsum, so that a human
/// looking at a benchmark site can tell at a glance that it is generated rather
/// than wondering whether they are looking at somebody's real content.
const WORDS: &[&str] = &[
    "server",
    "latency",
    "throughput",
    "memory",
    "storage",
    "network",
    "cache",
    "request",
    "response",
    "database",
    "index",
    "query",
    "thread",
    "process",
    "kernel",
    "buffer",
    "socket",
    "packet",
    "cluster",
    "region",
    "instance",
    "volume",
    "snapshot",
    "backup",
    "restore",
    "migration",
    "deployment",
    "release",
    "version",
    "rollback",
    "monitor",
    "metric",
    "alert",
    "threshold",
    "capacity",
    "workload",
    "benchmark",
    "baseline",
    "profile",
    "sample",
    "median",
    "average",
    "percentile",
    "variance",
    "outlier",
    "duration",
    "interval",
    "timestamp",
    "session",
    "cookie",
    "header",
    "payload",
    "encoding",
    "compression",
    "certificate",
    "handshake",
    "protocol",
    "endpoint",
    "gateway",
    "balancer",
    "replica",
    "primary",
    "failover",
    "quorum",
];

/// How much content a fixture holds.
///
/// Two sizes, and no way to ask for a third. A corpus size an operator could
/// choose would make two runs incomparable while looking like the same
/// benchmark - the same argument that gives `database.oltp` a fixed scale
/// factor. Which size a run used is a comparability key, not a tuning knob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixtureSize {
    /// Enough content to exercise the loop, the index and the archive pages
    /// without making a `quick` profile slow to set up.
    Small,
    /// A site with enough posts that pagination, taxonomy archives and the
    /// object cache all do real work. This is the size a comparable result is
    /// taken at.
    Standard,
}

impl FixtureSize {
    /// `(posts, pages, comments per commented post, authors, categories, tags)`
    ///
    /// Chosen so the standard corpus is roughly the shape of a small business
    /// site that has been running for a few years: a few hundred posts across a
    /// handful of categories, most with a few comments, a long tail with none.
    const fn counts(self) -> Counts {
        match self {
            Self::Small => Counts {
                posts: 30,
                pages: 4,
                authors: 2,
                categories: 4,
                tags: 12,
                max_comments: 3,
                paragraphs: 3,
            },
            Self::Standard => Counts {
                posts: 300,
                pages: 12,
                authors: 5,
                categories: 8,
                tags: 40,
                max_comments: 6,
                paragraphs: 6,
            },
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Standard => "standard",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Counts {
    posts: usize,
    pages: usize,
    authors: usize,
    categories: usize,
    tags: usize,
    max_comments: usize,
    paragraphs: usize,
}

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Author {
    pub login: String,
    pub display_name: String,
    pub email: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Term {
    pub slug: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    pub id: u32,
    pub author: String,
    pub email: String,
    pub body: String,
    pub date: String,
    /// `0` for a top-level comment, otherwise the id it replies to.
    ///
    /// Threading is not decoration: a comment tree is what makes WordPress walk
    /// a recursive structure per page rather than a flat list, and a fixture
    /// without one would let a theme's comment loop look cheaper than it is.
    pub parent: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Post {
    pub id: u32,
    pub title: String,
    pub slug: String,
    pub body: String,
    pub excerpt: String,
    pub date: String,
    pub author: String,
    pub category: String,
    pub tags: Vec<String>,
    pub comments: Vec<Comment>,
    /// `post` or `page`.
    pub kind: &'static str,
}

/// A complete, deterministic WordPress corpus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fixture {
    pub size: FixtureSize,
    pub authors: Vec<Author>,
    pub categories: Vec<Term>,
    pub tags: Vec<Term>,
    pub posts: Vec<Post>,
}

impl Fixture {
    /// Builds the corpus. Identical on every machine, every time.
    pub fn generate(size: FixtureSize) -> Self {
        let counts = size.counts();
        // One generator, drawn from in a fixed order. Two generators, or an
        // order that depended on anything, would make the corpus depend on
        // evaluation order rather than on the seed.
        let mut rng = SplitMix64::new(FIXTURE_SEED);

        let authors: Vec<Author> = (0..counts.authors)
            .map(|index| {
                let name = word(&mut rng);
                Author {
                    login: format!("{name}{index}"),
                    display_name: format!("{} {}", capitalise(name), capitalise(word(&mut rng))),
                    email: format!("{name}{index}@example.com"),
                }
            })
            .collect();

        let categories: Vec<Term> = (0..counts.categories)
            .map(|index| term(&mut rng, index, "topic"))
            .collect();
        let tags: Vec<Term> = (0..counts.tags)
            .map(|index| term(&mut rng, index, "tag"))
            .collect();

        let mut posts = Vec::with_capacity(counts.posts + counts.pages);
        let mut next_id: u32 = 1;
        let mut next_comment_id: u32 = 1;

        for index in 0..counts.posts {
            let post = build_post(
                &mut rng,
                &mut next_id,
                &mut next_comment_id,
                index,
                "post",
                &counts,
                &authors,
                &categories,
                &tags,
            );
            posts.push(post);
        }
        for index in 0..counts.pages {
            let post = build_post(
                &mut rng,
                &mut next_id,
                &mut next_comment_id,
                index,
                "page",
                &counts,
                &authors,
                &categories,
                &tags,
            );
            posts.push(post);
        }

        Self {
            size,
            authors,
            categories,
            tags,
            posts,
        }
    }

    /// Posts, excluding pages.
    pub fn post_count(&self) -> usize {
        self.posts.iter().filter(|p| p.kind == "post").count()
    }

    pub fn page_count(&self) -> usize {
        self.posts.iter().filter(|p| p.kind == "page").count()
    }

    pub fn comment_count(&self) -> usize {
        self.posts.iter().map(|p| p.comments.len()).sum()
    }

    /// SHA-256 of the WXR document, as lowercase hex.
    ///
    /// This is what makes "the same content on every machine" checkable rather
    /// than asserted. It goes in the bundle as a comparability key, so two
    /// results whose fixtures differ can be refused by the comparison layer
    /// instead of being averaged into a meaningless difference.
    pub fn checksum(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.to_wxr().as_bytes());
        hex::encode(hasher.finalize())
    }

    /// Renders the corpus as a WordPress eXtended RSS document.
    pub fn to_wxr(&self) -> String {
        // Pre-sized generously; a standard corpus is a couple of megabytes and
        // growing a String from empty would reallocate a dozen times.
        let mut out = String::with_capacity(4 << 20);
        out.push_str(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!-- Generated by DARCBench. Deterministic benchmark content; not a real site. -->\n\
             <rss version=\"2.0\"\n\
             \x20   xmlns:content=\"http://purl.org/rss/1.0/modules/content/\"\n\
             \x20   xmlns:dc=\"http://purl.org/dc/elements/1.1/\"\n\
             \x20   xmlns:wp=\"http://wordpress.org/export/1.2/\">\n\
             <channel>\n",
        );
        out.push_str(&format!(
            "  <title>DARCBench fixture {version} ({size})</title>\n\
             \x20 <link>{SITE_URL}</link>\n\
             \x20 <description>Deterministic benchmark content</description>\n\
             \x20 <language>en-US</language>\n\
             \x20 <wp:wxr_version>1.2</wp:wxr_version>\n\
             \x20 <wp:base_site_url>{SITE_URL}</wp:base_site_url>\n\
             \x20 <wp:base_blog_url>{SITE_URL}</wp:base_blog_url>\n",
            version = FIXTURE_VERSION,
            size = self.size.as_str(),
        ));

        for (index, author) in self.authors.iter().enumerate() {
            out.push_str(&format!(
                "  <wp:author>\n\
                 \x20   <wp:author_id>{id}</wp:author_id>\n\
                 \x20   <wp:author_login>{login}</wp:author_login>\n\
                 \x20   <wp:author_email>{email}</wp:author_email>\n\
                 \x20   <wp:author_display_name>{name}</wp:author_display_name>\n\
                 \x20 </wp:author>\n",
                id = index + 1,
                login = cdata(&author.login),
                email = cdata(&author.email),
                name = cdata(&author.display_name),
            ));
        }
        for (kind, terms) in [("category", &self.categories), ("post_tag", &self.tags)] {
            for term in terms.iter() {
                let element = if kind == "category" {
                    "category"
                } else {
                    "tag"
                };
                out.push_str(&format!(
                    "  <wp:{element}>\n\
                     \x20   <wp:{element}_slug>{slug}</wp:{element}_slug>\n\
                     \x20   <wp:cat_name>{name}</wp:cat_name>\n\
                     \x20 </wp:{element}>\n",
                    slug = escape_xml(&term.slug),
                    name = cdata(&term.name),
                ));
            }
        }

        for post in &self.posts {
            out.push_str(&self.render_item(post));
        }

        out.push_str("</channel>\n</rss>\n");
        out
    }

    fn render_item(&self, post: &Post) -> String {
        let mut item = format!(
            "  <item>\n\
             \x20   <title>{title}</title>\n\
             \x20   <link>{SITE_URL}/{slug}/</link>\n\
             \x20   <dc:creator>{author}</dc:creator>\n\
             \x20   <guid isPermaLink=\"false\">{SITE_URL}/?p={id}</guid>\n\
             \x20   <description></description>\n\
             \x20   <content:encoded>{body}</content:encoded>\n\
             \x20   <excerpt:encoded>{excerpt}</excerpt:encoded>\n\
             \x20   <wp:post_id>{id}</wp:post_id>\n\
             \x20   <wp:post_date>{date}</wp:post_date>\n\
             \x20   <wp:post_name>{slug}</wp:post_name>\n\
             \x20   <wp:status>publish</wp:status>\n\
             \x20   <wp:post_type>{kind}</wp:post_type>\n\
             \x20   <wp:comment_status>open</wp:comment_status>\n",
            title = cdata(&post.title),
            slug = escape_xml(&post.slug),
            author = cdata(&post.author),
            id = post.id,
            body = cdata(&post.body),
            excerpt = cdata(&post.excerpt),
            date = escape_xml(&post.date),
            kind = post.kind,
        );

        if post.kind == "post" {
            item.push_str(&format!(
                "    <category domain=\"category\" nicename=\"{slug}\">{name}</category>\n",
                slug = escape_xml(&post.category),
                name = cdata(&post.category),
            ));
            for tag in &post.tags {
                item.push_str(&format!(
                    "    <category domain=\"post_tag\" nicename=\"{slug}\">{name}</category>\n",
                    slug = escape_xml(tag),
                    name = cdata(tag),
                ));
            }
        }

        for comment in &post.comments {
            item.push_str(&format!(
                "    <wp:comment>\n\
                 \x20     <wp:comment_id>{id}</wp:comment_id>\n\
                 \x20     <wp:comment_author>{author}</wp:comment_author>\n\
                 \x20     <wp:comment_author_email>{email}</wp:comment_author_email>\n\
                 \x20     <wp:comment_date>{date}</wp:comment_date>\n\
                 \x20     <wp:comment_content>{body}</wp:comment_content>\n\
                 \x20     <wp:comment_approved>1</wp:comment_approved>\n\
                 \x20     <wp:comment_parent>{parent}</wp:comment_parent>\n\
                 \x20   </wp:comment>\n",
                id = comment.id,
                author = cdata(&comment.author),
                email = cdata(&comment.email),
                date = escape_xml(&comment.date),
                body = cdata(&comment.body),
                parent = comment.parent,
            ));
        }

        item.push_str("  </item>\n");
        item
    }
}

// ---------------------------------------------------------------------------
// Generation helpers
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn build_post(
    rng: &mut SplitMix64,
    next_id: &mut u32,
    next_comment_id: &mut u32,
    index: usize,
    kind: &'static str,
    counts: &Counts,
    authors: &[Author],
    categories: &[Term],
    tags: &[Term],
) -> Post {
    let id = *next_id;
    *next_id += 1;

    let title = format!(
        "{} {} {}",
        capitalise(word(rng)),
        word(rng),
        capitalise(word(rng))
    );
    let slug = format!("{}-{}-{id}", word(rng), word(rng));
    let body = paragraphs(rng, counts.paragraphs);
    let excerpt = sentence(rng, 18);

    // Dates walk backwards from a fixed point, so the archive pages have a
    // realistic spread and the corpus does not depend on the wall clock -
    // which would make every run's fixture different and every checksum
    // useless.
    let date = fixed_date(index);

    let author = pick(authors, rng).display_name.clone();
    let category = pick(categories, rng).slug.clone();
    let tag_count = 1 + (next(rng) as usize % 3);
    let mut post_tags: Vec<String> = Vec::with_capacity(tag_count);
    for _ in 0..tag_count {
        let candidate = pick(tags, rng).slug.clone();
        if !post_tags.contains(&candidate) {
            post_tags.push(candidate);
        }
    }

    // A long tail with no comments at all, because a corpus where every post
    // is commented would let a theme's comment query look uniformly expensive
    // and hide the case an operator actually has.
    let comment_count = if index % 3 == 0 {
        0
    } else {
        1 + (next(rng) as usize % counts.max_comments)
    };
    let mut comments = Vec::with_capacity(comment_count);
    let mut first_id = 0u32;
    for position in 0..comment_count {
        let cid = *next_comment_id;
        *next_comment_id += 1;
        if position == 0 {
            first_id = cid;
        }
        comments.push(Comment {
            id: cid,
            author: capitalise(word(rng)).to_string(),
            email: format!("{}@example.com", word(rng)),
            body: sentence(rng, 24),
            date: fixed_date(index + position + 1),
            // Every third comment replies to the first, so the tree has depth
            // and WordPress walks a recursive structure rather than a list.
            parent: if position > 0 && position % 3 == 0 {
                first_id
            } else {
                0
            },
        });
    }

    Post {
        id,
        title,
        slug,
        body,
        excerpt,
        date,
        author,
        category,
        tags: post_tags,
        comments,
        kind,
    }
}

fn term(rng: &mut SplitMix64, index: usize, prefix: &str) -> Term {
    let name = word(rng);
    Term {
        slug: format!("{prefix}-{name}-{index}"),
        name: capitalise(name).to_string(),
    }
}

fn next(rng: &mut SplitMix64) -> u64 {
    rng.next_u64()
}

fn word(rng: &mut SplitMix64) -> &'static str {
    // `WORDS` is a non-empty compile-time constant, so the modulo is always in
    // range. Written with a fallback rather than an index so the invariant is
    // enforced by the type system instead of by a comment.
    let index = (next(rng) as usize) % WORDS.len();
    WORDS.get(index).copied().unwrap_or("server")
}

fn pick<'a, T>(items: &'a [T], rng: &mut SplitMix64) -> &'a T {
    // Callers only ever pass non-empty slices built from `Counts`, whose fields
    // are all non-zero constants. `unwrap_or` on the first element keeps that
    // from being a panic if a future size ever has none.
    let index = (next(rng) as usize) % items.len().max(1);
    items.get(index).unwrap_or_else(|| {
        // Unreachable for any `Counts` in this file; a slice with no first
        // element cannot exist because `items.len().max(1)` would have panicked
        // on the modulo first. Written so this function has no `unwrap`.
        #[allow(clippy::expect_used)]
        items
            .first()
            .expect("a fixture never has an empty term list")
    })
}

fn sentence(rng: &mut SplitMix64, words: usize) -> String {
    let mut out = String::with_capacity(words * 9);
    for position in 0..words {
        if position > 0 {
            out.push(' ');
        }
        let w = word(rng);
        if position == 0 {
            out.push_str(&capitalise(w));
        } else {
            out.push_str(w);
        }
    }
    out.push('.');
    out
}

/// The post body: HTML paragraphs, because that is what WordPress stores.
///
/// The only markup this module emits, and it is emitted from constants here
/// rather than assembled anywhere near generated text - so there is no place
/// for a tag to arrive from.
fn paragraphs(rng: &mut SplitMix64, count: usize) -> String {
    let mut out = String::with_capacity(count * 400);
    for index in 0..count {
        if index > 0 {
            out.push_str("\n\n");
        }
        out.push_str("<p>");
        for sentence_index in 0..3 {
            if sentence_index > 0 {
                out.push(' ');
            }
            // Drawn before the call, not inside its argument list: the
            // generator is a single sequence and evaluation order deciding
            // which draw is which is exactly the dependency this module's
            // determinism must not have.
            let length = 12 + (next(rng) as usize % 10);
            out.push_str(&sentence(rng, length));
        }
        out.push_str("</p>");
    }
    out
}

fn capitalise(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// A date derived from an index rather than from the clock.
///
/// The wall clock is the one input that would make every run's fixture
/// different and every checksum useless, so it is not read. Posts walk
/// backwards a day at a time from a fixed point, which gives the archive pages
/// a realistic spread without introducing a source of variation.
fn fixed_date(index: usize) -> String {
    // 2024-12-31, then backwards. Arithmetic on a proleptic 30-day month:
    // WordPress parses these as dates and never as an interval, so the only
    // properties that matter are that they are valid, ordered and stable.
    let day_offset = index % 28;
    let month_offset = (index / 28) % 12;
    let year = 2024 - (index / (28 * 12)) as i32;
    format!(
        "{year:04}-{:02}-{:02} {:02}:{:02}:00",
        12 - month_offset,
        28 - day_offset,
        (index % 24),
        (index * 7) % 60,
    )
}

// ---------------------------------------------------------------------------
// XML safety
// ---------------------------------------------------------------------------

/// Wraps text in a CDATA section, splitting any sequence that would end it.
///
/// `]]>` is the only sequence that can terminate a CDATA section, and the only
/// way out of one. The generator cannot currently produce it - every string
/// comes from [`WORDS`] and integers - but this is not written on the
/// assumption that it never will be. A word list is the kind of thing somebody
/// extends, and the failure mode of extending it carelessly is markup in a CMS
/// on every machine that runs the benchmark.
fn cdata(text: &str) -> String {
    format!("<![CDATA[{}]]>", text.replace("]]>", "]]]]><![CDATA[>"))
}

/// Escapes the five XML entities, for the places CDATA cannot go.
///
/// Attribute values and elements this format does not wrap. The apostrophe is
/// escaped as `&apos;` rather than left alone because an attribute may be
/// single-quoted, and a generator that only handled the double-quoted case
/// would be correct until somebody changed a quote.
fn escape_xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The checksum of the standard corpus, pinned.
    ///
    /// This is the test that makes "deterministic content" a property rather
    /// than a promise. Any change to the generator - a word, an order, a count,
    /// a date - moves this hash, and moving it without bumping
    /// [`FIXTURE_VERSION`] would silently make every historical `wordpress.*`
    /// comparison wrong while every artifact still claimed the same fixture.
    ///
    /// If this test fails and the change was intended: bump the version, then
    /// update the hash. In that order, and never the other way round.
    const STANDARD_CHECKSUM: &str =
        "a6d33e85264906f9927e388792e5c0e060369e4e0ad9cef056b3d9f5d2b80e5d";

    #[test]
    fn the_standard_corpus_has_a_stable_checksum() {
        let checksum = Fixture::generate(FixtureSize::Standard).checksum();
        assert_eq!(
            checksum, STANDARD_CHECKSUM,
            "\nThe fixture's content changed. If that was intended, bump FIXTURE_VERSION \
             ({FIXTURE_VERSION}) FIRST and then set STANDARD_CHECKSUM to:\n  {checksum}\n\
             Changing the hash without the version silently makes every historical \
             wordpress.* comparison wrong while every artifact still claims the same fixture.\n"
        );
    }

    #[test]
    fn generating_twice_produces_the_same_corpus() {
        // The property the checksum rests on: no clock, no randomness, no
        // environment. Compared structurally as well as by hash, so a failure
        // says which field moved.
        for size in [FixtureSize::Small, FixtureSize::Standard] {
            assert_eq!(Fixture::generate(size), Fixture::generate(size));
            assert_eq!(
                Fixture::generate(size).checksum(),
                Fixture::generate(size).checksum()
            );
        }
    }

    #[test]
    fn the_two_sizes_are_different_corpora() {
        let small = Fixture::generate(FixtureSize::Small);
        let standard = Fixture::generate(FixtureSize::Standard);
        assert_ne!(small.checksum(), standard.checksum());
        assert!(standard.post_count() > small.post_count());
        assert_eq!(small.post_count(), 30);
        assert_eq!(standard.post_count(), 300);
        assert_eq!(standard.page_count(), 12);
    }

    #[test]
    fn nothing_generated_can_escape_the_xml_it_is_written_into() {
        // The structural claim in the module docs, checked against the actual
        // output rather than against the word list. Every string in a fixture
        // comes from WORDS and integers, so none of these can appear - and
        // asserting on the output is what keeps that true if the vocabulary
        // ever grows.
        let fixture = Fixture::generate(FixtureSize::Standard);
        for post in &fixture.posts {
            for text in [&post.title, &post.slug, &post.excerpt, &post.author] {
                assert!(!text.contains('<'), "{text:?}");
                assert!(!text.contains('&'), "{text:?}");
                assert!(!text.contains("]]>"), "{text:?}");
                assert!(text.is_ascii(), "{text:?}");
            }
            // The body is the one place markup is emitted, and only from
            // constants in `paragraphs`.
            assert!(post.body.starts_with("<p>"));
            assert!(!post.body.contains("<script"));
            assert!(!post.body.contains("]]>"));
            for comment in &post.comments {
                assert!(!comment.body.contains('<'));
                assert!(!comment.author.contains('<'));
            }
        }
    }

    #[test]
    fn a_cdata_section_cannot_be_terminated_from_inside_it() {
        // Tested against strings the generator cannot currently produce,
        // precisely so a future word list cannot quietly make them reachable.
        let hostile = "before]]><script>alert(1)</script><![CDATA[after";
        let wrapped = cdata(hostile);
        // Exactly one opening and, after the split, no bare terminator that
        // leaves the section early with attacker text outside it.
        assert!(wrapped.starts_with("<![CDATA["));
        assert!(wrapped.ends_with("]]>"));
        assert!(!wrapped.contains("]]><script>"), "{wrapped}");
        assert!(wrapped.contains("]]]]><![CDATA[>"), "{wrapped}");

        // Ordinary text is left alone.
        assert_eq!(cdata("hello world"), "<![CDATA[hello world]]>");
    }

    #[test]
    fn escaping_covers_the_single_quoted_attribute_case_too() {
        // A generator that only handled double quotes would be correct until
        // somebody changed a quote in the template above.
        assert_eq!(
            escape_xml("a<b>c&d\"e'f"),
            "a&lt;b&gt;c&amp;d&quot;e&apos;f"
        );
    }

    #[test]
    fn the_document_is_balanced_and_well_formed_enough_to_import() {
        // Not a full XML parse - this workspace has no XML parser and adding
        // one to test a generator would be adding a dependency to check a
        // property a count can check. Element balance and the CDATA invariant
        // are what actually break an import.
        let wxr = Fixture::generate(FixtureSize::Small).to_wxr();
        assert!(wxr.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(wxr.trim_end().ends_with("</rss>"));

        for (open, close) in [
            ("<item>", "</item>"),
            ("<channel>", "</channel>"),
            ("<wp:comment>", "</wp:comment>"),
            ("<wp:author>", "</wp:author>"),
            ("<content:encoded>", "</content:encoded>"),
        ] {
            assert_eq!(
                wxr.matches(open).count(),
                wxr.matches(close).count(),
                "unbalanced {open}"
            );
        }
        // Every CDATA section is closed.
        assert_eq!(
            wxr.matches("<![CDATA[").count(),
            wxr.matches("]]>").count(),
            "a CDATA section was left open"
        );
    }

    #[test]
    fn every_item_in_the_document_is_a_post_or_a_page() {
        let fixture = Fixture::generate(FixtureSize::Small);
        let wxr = fixture.to_wxr();
        assert_eq!(
            wxr.matches("<item>").count(),
            fixture.post_count() + fixture.page_count()
        );
        assert_eq!(
            wxr.matches("<wp:post_type>post</wp:post_type>").count(),
            fixture.post_count()
        );
        assert_eq!(
            wxr.matches("<wp:post_type>page</wp:post_type>").count(),
            fixture.page_count()
        );
    }

    #[test]
    fn post_ids_and_comment_ids_are_unique() {
        // WordPress keys on these, so a collision silently merges two posts and
        // the corpus is quietly smaller than every artifact says it is.
        let fixture = Fixture::generate(FixtureSize::Standard);
        let mut post_ids: Vec<u32> = fixture.posts.iter().map(|p| p.id).collect();
        let total_posts = post_ids.len();
        post_ids.sort_unstable();
        post_ids.dedup();
        assert_eq!(post_ids.len(), total_posts);

        let mut comment_ids: Vec<u32> = fixture
            .posts
            .iter()
            .flat_map(|p| p.comments.iter().map(|c| c.id))
            .collect();
        let total_comments = comment_ids.len();
        comment_ids.sort_unstable();
        comment_ids.dedup();
        assert_eq!(comment_ids.len(), total_comments);
    }

    #[test]
    fn the_comment_tree_has_depth_and_a_long_tail_without_any() {
        // A flat comment list lets a theme's comment loop look cheaper than it
        // is; a corpus where every post is commented hides the case an operator
        // actually has.
        let fixture = Fixture::generate(FixtureSize::Standard);
        assert!(fixture.comment_count() > 0);

        let replies = fixture
            .posts
            .iter()
            .flat_map(|p| p.comments.iter())
            .filter(|c| c.parent != 0)
            .count();
        assert!(replies > 0, "no threaded comments; the tree has no depth");

        let uncommented = fixture
            .posts
            .iter()
            .filter(|p| p.comments.is_empty())
            .count();
        assert!(uncommented > 0, "every post is commented");
        assert!(uncommented < fixture.posts.len(), "no post is commented");

        // A reply must point at a comment that exists on the same post.
        for post in &fixture.posts {
            let ids: Vec<u32> = post.comments.iter().map(|c| c.id).collect();
            for comment in &post.comments {
                if comment.parent != 0 {
                    assert!(
                        ids.contains(&comment.parent),
                        "comment {} replies to {}, which is not on this post",
                        comment.id,
                        comment.parent
                    );
                }
            }
        }
    }

    #[test]
    fn no_generated_address_belongs_to_anyone() {
        // RFC 2606 reserves example.com precisely so that it cannot. A fixture
        // naming a real domain would put it in every imported database and
        // every rendered link on every machine that ran the benchmark.
        let fixture = Fixture::generate(FixtureSize::Standard);
        assert!(SITE_URL.contains("example.com"));
        for author in &fixture.authors {
            assert!(author.email.ends_with("@example.com"), "{}", author.email);
        }
        for comment in fixture.posts.iter().flat_map(|p| p.comments.iter()) {
            assert!(comment.email.ends_with("@example.com"), "{}", comment.email);
        }
    }

    #[test]
    fn the_corpus_does_not_depend_on_the_wall_clock() {
        // The one input that would make every run's fixture different and
        // every checksum useless. Asserted by the dates being in the past and
        // fixed, rather than by trying to move the clock.
        let fixture = Fixture::generate(FixtureSize::Standard);
        for post in &fixture.posts {
            assert!(post.date.starts_with("202") || post.date.starts_with("201"));
            // `YYYY-MM-DD HH:MM:SS`, which is what WXR expects.
            assert_eq!(post.date.len(), 19, "{}", post.date);
            let (date, time) = post.date.split_once(' ').unwrap();
            let parts: Vec<&str> = date.split('-').collect();
            assert_eq!(parts.len(), 3);
            let month: u32 = parts[1].parse().unwrap();
            let day: u32 = parts[2].parse().unwrap();
            assert!((1..=12).contains(&month), "{}", post.date);
            assert!((1..=28).contains(&day), "{}", post.date);
            let hour: u32 = time.split(':').next().unwrap().parse().unwrap();
            assert!(hour < 24, "{}", post.date);
        }
    }

    #[test]
    fn a_standard_corpus_is_large_enough_to_be_worth_measuring() {
        // A WordPress serving twelve posts and one serving twelve hundred are
        // different programs at runtime. The standard corpus has to be big
        // enough that pagination, taxonomy archives and the object cache all do
        // real work.
        let fixture = Fixture::generate(FixtureSize::Standard);
        let wxr = fixture.to_wxr();
        assert!(fixture.post_count() >= 200);
        assert!(fixture.comment_count() >= 200);
        assert!(
            wxr.len() > 512 * 1024,
            "the standard corpus is only {} bytes",
            wxr.len()
        );
        // And small enough that importing it is not the benchmark.
        assert!(wxr.len() < 16 * 1024 * 1024, "{} bytes", wxr.len());
    }
}
