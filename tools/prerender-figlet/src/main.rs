fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    if args.len() != 3 {
        eprintln!("usage: dustnet-prerender-figlet <site-source> <site-output>");
        std::process::exit(2);
    }
    if let Err(error) = dustnet_prerender_figlet::build_site(
        std::path::Path::new(&args[1]),
        std::path::Path::new(&args[2]),
    ) {
        eprintln!("site build failed: {error}");
        std::process::exit(1);
    }
}
