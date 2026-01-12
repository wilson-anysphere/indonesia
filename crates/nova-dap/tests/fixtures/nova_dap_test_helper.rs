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
    let mut print_line_len: Option<usize> = None;
    let mut print_line_len_stderr: Option<usize> = None;
    let mut heartbeat: bool = false;
    let mut heartbeat_file_path: Option<String> = None;
    let mut spam_stdout_lines: u64 = 0;
    let mut spam_stderr_lines: u64 = 0;

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
            "--print-line-len" => {
                print_line_len = args.next().and_then(|v| v.parse::<usize>().ok());
            }
            "--print-line-len-stderr" => {
                print_line_len_stderr = args.next().and_then(|v| v.parse::<usize>().ok());
            }
            "--heartbeat" => heartbeat = true,
            "--heartbeat-file" => {
                heartbeat_file_path = args.next();
                heartbeat = true;
            }
            "--spam-stdout-lines" => {
                spam_stdout_lines = args
                    .next()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(spam_stdout_lines);
            }
            "--spam-stderr-lines" => {
                spam_stderr_lines = args
                    .next()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(spam_stderr_lines);
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

    if let Some(len) = print_line_len {
        let mut out = io::stdout().lock();
        write_repeated(&mut out, b'a', len).expect("write long stdout line");
        writeln!(&mut out).expect("write long stdout newline");
        out.flush().expect("flush long stdout line");
    }

    if let Some(len) = print_line_len_stderr {
        let mut err = io::stderr().lock();
        write_repeated(&mut err, b'b', len).expect("write long stderr line");
        writeln!(&mut err).expect("write long stderr newline");
        err.flush().expect("flush long stderr line");
    }

    if spam_stdout_lines > 0 {
        let mut stdout = io::BufWriter::new(io::stdout());
        for idx in 0..spam_stdout_lines {
            let _ = writeln!(
                stdout,
                "nova-dap test helper stdout spam idx={idx} pid={pid}"
            );
        }
        let _ = stdout.flush();
    }
    if spam_stderr_lines > 0 {
        let mut stderr = io::BufWriter::new(io::stderr());
        for idx in 0..spam_stderr_lines {
            let _ = writeln!(
                stderr,
                "nova-dap test helper stderr spam idx={idx} pid={pid}"
            );
        }
        let _ = stderr.flush();
    }

    if let Some(exit_after_ms) = exit_after_ms {
        thread::sleep(Duration::from_millis(exit_after_ms));
        println!("nova-dap test helper exiting pid={pid} code={exit_code}");
        let _ = io::stdout().flush();
        std::process::exit(exit_code);
    }

    let mut heartbeat_file = heartbeat_file_path.as_deref().and_then(|path| {
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
    });

    loop {
        thread::sleep(Duration::from_millis(sleep_ms));
        if heartbeat {
            // When the adapter is launched in stdio mode, stdout/stderr are pipes.
            // If the adapter detaches and exits, those pipes are closed and future writes will
            // fail. This helper is used in detach tests, so we ignore write errors here.
            let mut out = io::stdout().lock();
            let _ = writeln!(&mut out, "nova-dap test helper heartbeat pid={pid}");
            let _ = out.flush();

            let mut err = io::stderr().lock();
            let _ = writeln!(&mut err, "nova-dap test helper heartbeat stderr pid={pid}");
            let _ = err.flush();
        }

        if let Some(file) = heartbeat_file.as_mut() {
            let _ = writeln!(file, "heartbeat pid={pid}");
            let _ = file.flush();
        }
    }
}

fn write_repeated<W: Write>(writer: &mut W, byte: u8, count: usize) -> io::Result<()> {
    let chunk = [byte; 8192];
    let mut remaining = count;
    while remaining > 0 {
        let n = remaining.min(chunk.len());
        writer.write_all(&chunk[..n])?;
        remaining -= n;
    }
    Ok(())
}
