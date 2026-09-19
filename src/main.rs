use std::process::ExitCode;
use uiw::server::HttpServer;
use uiw::service::App;

struct Args {
    listen: String,
    data_dir: String,
}

fn parse_args(argv: Vec<String>) -> Result<Args, String> {
    let mut listen = "127.0.0.1:5213".to_string();
    let mut data_dir = "workbench-data".to_string();
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "--listen" => {
                i += 1;
                listen = argv.get(i).ok_or("missing value for --listen")?.clone();
            }
            "--data-dir" => {
                i += 1;
                data_dir = argv.get(i).ok_or("missing value for --data-dir")?.clone();
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }
    Ok(Args { listen, data_dir })
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().collect()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("usage: server --listen 127.0.0.1:5213 [--data-dir PATH]\n{e}");
            return ExitCode::FAILURE;
        }
    };
    let app = match App::open(&args.data_dir) {
        Ok(app) => app,
        Err(e) => {
            eprintln!("failed to open data dir {}: {e}", args.data_dir);
            return ExitCode::FAILURE;
        }
    };
    let server = HttpServer::new(app);
    if let Err(e) = server.run(&args.listen) {
        eprintln!("server error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
