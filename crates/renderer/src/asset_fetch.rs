//! 共享下载/校验工具（运行时使用：drawio webapp + headless-shell）。

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use sha2::Digest;

/// Maximum total time we'll spend on a single download.
pub const DOWNLOAD_OVERALL_TIMEOUT: Duration = Duration::from_secs(1800); // 30 min
/// Log progress every N bytes or every N seconds, whichever comes first.
pub const PROGRESS_LOG_BYTES: u64 = 10 * 1024 * 1024; // 10 MB
pub const PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// Stream-copy from `reader` to `writer` in 64KB chunks, logging progress to
/// cargo's warning stream (build) or stderr (runtime) and enforcing an
/// overall wall-clock timeout.
pub fn download_with_progress<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
) -> io::Result<()> {
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    let start = Instant::now();
    let mut last_log = Instant::now();
    let mut next_log_threshold: u64 = PROGRESS_LOG_BYTES;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        total += n as u64;
        let elapsed = start.elapsed();
        if elapsed > DOWNLOAD_OVERALL_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "{label}: overall timeout after {total} bytes ({:.1}s elapsed)",
                    elapsed.as_secs_f64()
                ),
            ));
        }
        if total >= next_log_threshold || last_log.elapsed() > PROGRESS_LOG_INTERVAL {
            eprintln!(
                "{label}: {} MB downloaded in {:.1}s",
                total / (1024 * 1024),
                elapsed.as_secs_f64(),
            );
            last_log = Instant::now();
            next_log_threshold = total.saturating_add(PROGRESS_LOG_BYTES);
        }
    }
    Ok(())
}

pub fn sha256_file(p: &Path) -> Option<String> {
    let mut f = File::open(p).ok()?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(format!("{:x}", hasher.finalize()))
}

