mod engine;
mod http;
mod service;
mod store;

use store::Store;

fn main() -> Result<(), String> {
    let mut listen = "127.0.0.1:5213".to_owned();
    let mut data_dir = "data/unicode-workbench".to_owned();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = args.next().ok_or("missing --listen value")?,
            "--data-dir" => data_dir = args.next().ok_or("missing --data-dir value")?,
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    let store = Store::open(data_dir)?;
    http::serve(store, &listen)
}
