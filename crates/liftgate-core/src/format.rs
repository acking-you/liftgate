//! Human-friendly unit formatters shared across the CLI and TUI.

/// Formats elapsed seconds into compact human units (`42s`, `5m 12s`, `2h 09m`, `1d 04h`).
#[must_use]
pub fn format_duration_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else if secs < 86_400 {
        format!("{}h {:02}m", secs / 3_600, (secs % 3_600) / 60)
    } else {
        format!("{}d {:02}h", secs / 86_400, (secs % 86_400) / 3_600)
    }
}

/// Formats millisecond latency into compact human units (`140ms`, `2.4s`, `1m 12s`).
#[must_use]
pub fn format_latency_ms(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        format_duration_secs(ms / 1_000)
    }
}

/// Formats token counts into compact readable units (`0`, `940`, `18.4k`, `1.25M`).
#[must_use]
pub fn format_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 1_000_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        format!("{:.2}M", tokens as f64 / 1_000_000.0)
    }
}

/// Formats byte counts into compact readable units (`512 B`, `42.0 KiB`, `14.20 MiB`, `1.25 GiB`).
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const KIB: f64 = 1024.0;
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GiB", (bytes as f64) / GIB)
    } else if bytes >= 1024 * 1024 {
        format!("{:.2} MiB", (bytes as f64) / MIB)
    } else if bytes >= 1024 {
        format!("{:.1} KiB", (bytes as f64) / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Formats a byte-per-second transfer rate (`0 B/s`, `840 B/s`, `512.0 KiB/s`, `18.42 MiB/s`).
#[must_use]
pub fn format_transfer_rate(bytes_per_sec: f64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const KIB: f64 = 1024.0;
    if !bytes_per_sec.is_finite() || bytes_per_sec <= 0.0 {
        "0 B/s".to_owned()
    } else if bytes_per_sec >= GIB {
        format!("{:.2} GiB/s", bytes_per_sec / GIB)
    } else if bytes_per_sec >= MIB {
        format!("{:.2} MiB/s", bytes_per_sec / MIB)
    } else if bytes_per_sec >= KIB {
        format!("{:.1} KiB/s", bytes_per_sec / KIB)
    } else {
        format!("{:.0} B/s", bytes_per_sec)
    }
}

/// Formats an estimated time remaining in seconds (`<1s`, `14s`, `2m 05s`).
#[must_use]
pub fn format_eta(secs: u64) -> String {
    if secs == 0 {
        "<1s".to_owned()
    } else {
        format_duration_secs(secs)
    }
}

/// Formats a Unix-millisecond timestamp relative to `now_ms` (`just now`, `14s ago`, `3m ago`).
#[must_use]
pub fn format_relative_ms(timestamp_ms: u64, now_ms: u64) -> String {
    if timestamp_ms == 0 {
        return "—".to_owned();
    }
    let delta_secs = now_ms.saturating_sub(timestamp_ms) / 1_000;
    if delta_secs < 5 {
        "just now".to_owned()
    } else if delta_secs < 60 {
        format!("{delta_secs}s ago")
    } else if delta_secs < 3_600 {
        format!("{}m ago", delta_secs / 60)
    } else if delta_secs < 86_400 {
        format!("{}h ago", delta_secs / 3_600)
    } else {
        format!("{}d ago", delta_secs / 86_400)
    }
}

/// Formats a Unix-millisecond timestamp as `HH:MM:SSZ`.
#[must_use]
pub fn format_utc_hms(timestamp_ms: u64) -> String {
    let total_secs = timestamp_ms / 1_000;
    let day_secs = total_secs % 86_400;
    let hours = day_secs / 3_600;
    let mins = (day_secs % 3_600) / 60;
    let secs = day_secs % 60;
    format!("{hours:02}:{mins:02}:{secs:02}Z")
}

/// Builds a compact Unicode bar (`▰▰▰▰▱▱▱▱`) representing `filled / total`.
#[must_use]
pub fn progress_bar(filled: usize, total: usize, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let clamped_total = total.max(1);
    let ratio = (filled.min(clamped_total) * width) / clamped_total;
    let active = "▰".repeat(ratio);
    let empty = "▱".repeat(width.saturating_sub(ratio));
    format!("{active}{empty}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations_tokens_bytes_and_relative_timestamps_cleanly() {
        assert_eq!(format_duration_secs(9), "9s");
        assert_eq!(format_duration_secs(125), "2m 05s");
        assert_eq!(format_duration_secs(7_380), "2h 03m");

        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(12_500), "12.5k");
        assert_eq!(format_tokens(1_500_000), "1.50M");

        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.00 GiB");

        assert_eq!(format_transfer_rate(0.0), "0 B/s");
        assert_eq!(format_transfer_rate(f64::NAN), "0 B/s");
        assert_eq!(format_transfer_rate(840.0), "840 B/s");
        assert_eq!(format_transfer_rate(512.0 * 1024.0), "512.0 KiB/s");
        assert_eq!(format_transfer_rate(18.5 * 1024.0 * 1024.0), "18.50 MiB/s");
        assert_eq!(format_eta(0), "<1s");
        assert_eq!(format_eta(14), "14s");
        assert_eq!(format_eta(125), "2m 05s");

        assert_eq!(format_relative_ms(10_000, 12_000), "just now");
        assert_eq!(format_relative_ms(10_000, 40_000), "30s ago");
        assert_eq!(format_relative_ms(10_000, 190_000), "3m ago");
        assert_eq!(format_utc_hms(0), "00:00:00Z");
    }
}
