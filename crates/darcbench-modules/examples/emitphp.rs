fn main() {
    let size = if std::env::args().nth(1).as_deref() == Some("standard") {
        darcbench_modules::wordpress_fixture::FixtureSize::Standard
    } else {
        darcbench_modules::wordpress_fixture::FixtureSize::Small
    };
    print!(
        "{}",
        darcbench_modules::wordpress_fixture::Fixture::generate(size).to_php_import()
    );
}
