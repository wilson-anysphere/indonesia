use std::{
    env,
    fs,
    io::{self, Write},
    thread,
    time::Duration,
};

fn main() {
    let mut pid_file: Option<String> = None;
    let mut sleep_ms: u64 = 1000;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--pid-file" => pid_file = args.next(),
            "--sleep-ms" => {
                sleep_ms = args
                    .next()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(sleep_ms);
            }
            _ => {}
        }
    }

    let pid = std::process::id();
    if let Some(path) = pid_file {
        fs::write(path, pid.to_string()).expect("write pid file");
    }

    println!("nova-dap test helper started pid={pid}");
    eprintln!("nova-dap test helper stderr started pid={pid}");
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();

    loop {
        thread::sleep(Duration::from_millis(sleep_ms));
    }
}

