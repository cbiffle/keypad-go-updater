use std::{path::PathBuf, time::Duration, io::{Read, ErrorKind}};

use anyhow::{Context, bail};
use clap::Parser;
use serialport::Parity;
use stm32_uart_boot::{Boot, Pid, Cmd};
use indicatif::ProgressBar;

#[derive(Parser)]
struct Flasher {
    #[clap(short)]
    port: String,
    /// Programming baud rate. Rates up to 115200 work well in practice, but may
    /// corrupt the "target's first words" printed after programming since we
    /// have to downshift to 19200 there.
    #[clap(short, default_value_t = 19_200)]
    baud_rate: u32,
    #[clap(short, long)]
    verbose: bool,
    /// Expected output baud rate of target. Must match firmware.
    #[clap(long, default_value_t = 19_200)]
    run_baud_rate: u32,

    archive: PathBuf,
}

const START_OF_FLASH: u32 = 0x0800_0000; // TODO: this should not be hardcoded.

fn main() -> anyhow::Result<()> {
    let args = Flasher::parse();

    let port = serialport::new(&args.port, args.baud_rate)
        .timeout(Duration::from_millis(500))
        .parity(Parity::Even)
        .open()
        .with_context(|| format!("opening serial port {}", args.port))?;
    let mut boot = Boot::new(port);

    boot.drain()
        .context("unable to drain serial port")?;
    boot.poke()
        .context("unable to contact bootloader (may not be running or on a different interface)")?;
    println!("Device is responding on {}", args.port);

    let info = boot.info().context("getting device info")?;
    let Some(pid) = info.product_id else {
        bail!("device did not report product ID info")
    };
    let Pid::Stm32(kind) = pid else {
        bail!("device reported unrecognized product ID");
    };

    let ar = std::fs::File::open(&args.archive)
        .with_context(|| format!("opening {}", args.archive.display()))?;
    let mut ar = zip::ZipArchive::new(ar)
        .with_context(|| format!("reading ZIP header from {}", args.archive.display()))?;

    let version = {
        let mut vent = ar.by_name("VERSION")
            .context("finding VERSION file in archive")?;
        let mut version = String::new();
        vent.read_to_string(&mut version)
            .context("reading contents of VERSION file")?;
        version
    };
    let version = version.trim();

    let image_name = format!("{kind:?}.bin");
    let mut entry = match ar.by_name(&image_name) {
        Err(zip::result::ZipError::FileNotFound) => {
            bail!("can't find {image_name} in archive");
        }
        r => r.context("reading ZIP file")?,
    };

    let mut image = vec![];
    entry.read_to_end(&mut image)
        .with_context(|| format!("reading archived file {image_name}"))?;

    drop(entry);
    drop(ar);

    println!("Detected model: {kind:?}");
    println!("Archive contains firmware for this model.");
    println!("Image length: {}", image.len());

    println!();
    print!("Global erase...");
    boot.drain()?;
    if info.command_support[Cmd::EraseMemory] {
        boot.do_erase_memory_global()?;
    } else if info.command_support[Cmd::ExtendedEraseMemory] {
        boot.do_extended_erase_memory_global()?;
    } else {
        bail!("No erase command supported??");
    }
    println!("done.");

    println!("Beginning write...");
    let bar = ProgressBar::new(image.len() as u64);
    let mut addr = START_OF_FLASH;
    for chunk in image.chunks_mut(256) {
        boot.do_write_memory(addr, chunk)?;
        addr += chunk.len() as u32;
        bar.inc(chunk.len() as u64);
    }
    bar.finish();

    println!("Verifying...");
    let bar = ProgressBar::new(image.len() as u64);
    let mut buffer = [0; 256];
    let mut addr = START_OF_FLASH;
    let mut issues = 0;
    for chunk in image.chunks(256) {
        let buf = &mut buffer[..chunk.len()];
        boot.do_read_memory(addr, buf)?;
        for (o, (sought, got)) in chunk.iter().zip(buf).enumerate() {
            if sought != got {
                issues += 1;
                if args.verbose {
                    let ba = addr + o as u32;
                    println!("mismatch at {ba:08x}: sought {sought:#x}, got {got:#x}");
                }
            }
        }
        addr += chunk.len() as u32;
        bar.inc(chunk.len() as u64);
    }
    bar.finish();

    if issues != 0 {
        bail!("memory contents failed to match at {issues} addresses.");
    }

    println!("Done!");
    println!();
    println!("Activating firmware...");

    boot.do_go(START_OF_FLASH)?;

    let mut port = boot.into_port();
    port.set_parity(Parity::None)?;
    if args.run_baud_rate != args.baud_rate {
        port.set_baud_rate(args.run_baud_rate)?;
        println!("Note: changing baud rate, data may be lost");
    }

    println!("GO command succeeded, checking output...");
    println!();

    let mut buffer = vec![];
    let mut buf = [0; 512];
    loop {
        match port.read(&mut buf) {
            Ok(n) => {
                if n == 0 {
                    panic!();
                }
                buffer.extend_from_slice(&buf[..n]);
            }
            Err(e) if e.kind() == ErrorKind::TimedOut => {
                break;
            }
            Err(e) => {
                bail!("{e}");
            }
        }
    }
    let text = std::str::from_utf8(&buffer)
        .context("Output from target was not UTF-8!")?;
    let expected_first_words = format!(
        "\r\n\
        \r\n\
        SETUP MODE\r\n\
        Firmware version: {version}\r\n\
        \r\n\
        Press+hold any keypad button.\r\n\
        Type ESC here if no more.\r\n"
    );
    if text != expected_first_words {
        println!("ERROR: target's first transmission unexpected: {text:?}");
        println!("Expected: {expected_first_words:?}");
        println!("--- BEGIN ESCAPED LINES ---");
        for line in text.lines() {
            println!("{line:?}");
        }
        println!("--- END ESCAPED LINES ---");
    } else {
        println!("OK");
    }

    Ok(())
}
