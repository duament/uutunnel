use clap::Parser;
use std::io;
use std::num::ParseIntError;

mod server;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, env, default_value = "[::]:20000")]
    listen: String,

    #[arg(short, long, env, default_value = "162.159.193.1:4500")]
    target: String,

    #[arg(short, long, env)]
    uu_server: String,

    #[arg(short, long, env, value_parser = parse_magic)]
    magic: [u8; 4],
}

fn parse_magic(s: &str) -> Result<[u8; 4], ParseIntError> {
    let mut result = [0_u8; 4];
    let s = s.trim_start_matches("0x");
    for n in 0..4 {
        let hex_str = {
            let len = (n + 1) * 2 - 1;
            if s.len() == len {
                let pos = s.len() - (n + 1) * 2 + 1;
                &s[pos..1]
            } else if s.len() > len {
                let pos = s.len() - (n + 1) * 2;
                &s[pos..2]
            } else {
                continue;
            }
        };
        result[4 - n - 1] = u8::from_str_radix(hex_str, 16)?;
    }
    Ok(result)
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let args = Args::parse();
    server::run(args).await?;
    Ok(())
}
