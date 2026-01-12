use std::{
    env, fs,
    io::{self, Write},
    thread,
    time::Duration,
};

fn main() {
    let mut pid_file: Option<String> = None;
    let mut sleep_ms: u64 = 1000;
    let mut exit_after_ms: Option<u64> = None;
    let mut exit_code: i32 = 0;

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
            "--exit-after-ms" => {
                exit_after_ms = args.next().and_then(|v| v.parse::<u64>().ok());
            }
            "--exit-code" => {
                exit_code = args
                    .next()
                    .and_then(|v| v.parse::<i32>().ok())
                    .unwrap_or(exit_code);
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

    if let Some(exit_after_ms) = exit_after_ms {
        thread::sleep(Duration::from_millis(exit_after_ms));
        println!("nova-dap test helper exiting pid={pid} code={exit_code}");
        let _ = io::stdout().flush();
        std::process::exit(exit_code);
    }

    loop {
        thread::sleep(Duration::from_millis(sleep_ms));
    }
}
