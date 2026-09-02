use std::fs::File;
use std::io;

fn main() -> io::Result<()> {
    let mut client = File::open("chan:hello")?;
    io::copy(&mut client, &mut io::stdout())?;

    Ok(())
}
