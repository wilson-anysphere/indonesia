use std::{
    env,
    io::{self, Write},
    process,
    thread,
    time::Duration,
};

fn parse_usize(value: Option<String>, flag: &str) -> usize {
    let value = value.unwrap_or_else(|| {
        eprintln!("missing value for {flag}");
        process::exit(2);
    });
    value.parse().unwrap_or_else(|_| {
        eprintln!("invalid usize for {flag}: {value}");
        process::exit(2);
    })
}

fn parse_u64(value: Option<String>, flag: &str) -> u64 {
    let value = value.unwrap_or_else(|| {
        eprintln!("missing value for {flag}");
        process::exit(2);
    });
    value.parse().unwrap_or_else(|_| {
        eprintln!("invalid u64 for {flag}: {value}");
        process::exit(2);
    })
}

fn write_repeated(mut writer: impl Write, mut bytes: usize, fill: u8) -> io::Result<()> {
    let buf = [fill; 8 * 1024];
    while bytes > 0 {
        let n = bytes.min(buf.len());
        writer.write_all(&buf[..n])?;
        bytes -= n;
    }
    writer.flush()
}

fn spawn_child_sleep(ms: u64) {
    let exe = env::current_exe().unwrap_or_else(|err| {
        eprintln!("failed to resolve current exe: {err}");
        process::exit(2);
    });

    let _child = process::Command::new(exe)
        .args(["--sleep-ms", &ms.to_string()])
        .spawn()
        .unwrap_or_else(|err| {
            eprintln!("failed to spawn child: {err}");
            process::exit(2);
        });
}

fn main() {
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--stdout-bytes" => {
                let bytes = parse_usize(args.next(), "--stdout-bytes");
                write_repeated(io::stdout().lock(), bytes, b'a').unwrap();
            }
            "--stderr-bytes" => {
                let bytes = parse_usize(args.next(), "--stderr-bytes");
                write_repeated(io::stderr().lock(), bytes, b'b').unwrap();
            }
            "--sleep-ms" => {
                let ms = parse_u64(args.next(), "--sleep-ms");
                thread::sleep(Duration::from_millis(ms));
            }
            "--spawn-child-sleep-ms" => {
                let ms = parse_u64(args.next(), "--spawn-child-sleep-ms");
                spawn_child_sleep(ms);
            }
            other => {
                eprintln!("unknown argument: {other}");
                process::exit(2);
            }
        }
    }
}

