use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::process::Stdio;
use std::time::Duration;

use liftgate_types::config::ProxyUrl;
use tokio::process::Command;
use tokio::time::timeout;

use crate::platform::configure_acp_child_env;

const MAX_FETCH_TIMEOUT_SECS: u64 = 20;
const MAX_RAW_BYTES_STR: &str = "2097152";
const MAX_EXTRACTED_CHARS: usize = 36_000;

/// Outcome of a server-side `web_fetch` execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebFetchOutcome {
    Success {
        url: String,
        title: Option<String>,
        media_type: String,
        content: String,
    },
    Error {
        url: String,
        error_code: &'static str,
        message: String,
    },
}

/// Validates `raw_url` against SSRF & domain policy rules, fetches the URL asynchronously,
/// and extracts clean UTF-8 text suitable for `web_fetch_tool_result` and ACP continuation.
pub async fn fetch_url_sanitized(
    raw_url: &str,
    allowed_domains: &[String],
    blocked_domains: &[String],
) -> WebFetchOutcome {
    fetch_url_sanitized_with_proxy(raw_url, allowed_domains, blocked_domains, None).await
}

/// Like [`fetch_url_sanitized`], honoring Liftgate's explicit `agy_proxy` policy (`None` = direct).
pub async fn fetch_url_sanitized_with_proxy(
    raw_url: &str,
    allowed_domains: &[String],
    blocked_domains: &[String],
    agy_proxy: Option<&ProxyUrl>,
) -> WebFetchOutcome {
    let trimmed = raw_url.trim();
    let parsed = match validate_fetch_url(trimmed, allowed_domains, blocked_domains) {
        Ok(u) => u,
        Err((code, msg)) => {
            return WebFetchOutcome::Error {
                url: trimmed.to_owned(),
                error_code: code,
                message: msg,
            };
        }
    };

    let curl_bin = if cfg!(windows) { "curl.exe" } else { "curl" };
    let mut cmd = Command::new(curl_bin);
    cmd.args([
        "-sS",
        "-L",
        "--proto",
        "=http,https",
        "--proto-redir",
        "=http,https",
        "--max-redirs",
        "5",
        "--max-time",
        "18",
        "--max-filesize",
        MAX_RAW_BYTES_STR,
        "-A",
        "Mozilla/5.0 (compatible; AgyAcpGateway/0.1; +https://antigravity.google)",
        "-w",
        "\n__AGY_CURL_META__:%{http_code}\t%{content_type}",
        &parsed.normalized_url,
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    configure_acp_child_env(&mut cmd, agy_proxy);

    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let output = match timeout(Duration::from_secs(MAX_FETCH_TIMEOUT_SECS), cmd.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(err)) => {
            return WebFetchOutcome::Error {
                url: parsed.normalized_url,
                error_code: "url_not_accessible",
                message: format!("failed to invoke HTTP fetcher: {err}"),
            };
        }
        Err(_) => {
            return WebFetchOutcome::Error {
                url: parsed.normalized_url,
                error_code: "url_not_accessible",
                message: "request timed out while fetching URL".to_owned(),
            };
        }
    };

    let raw_stdout = String::from_utf8_lossy(&output.stdout);
    let (body_part, status_code, content_type) = split_curl_trailer(&raw_stdout);

    if !output.status.success() && status_code == 0 {
        let stderr_msg = String::from_utf8_lossy(&output.stderr);
        let code = if stderr_msg.contains("Maximum file size exceeded") {
            "content_too_large"
        } else {
            "url_not_accessible"
        };
        return WebFetchOutcome::Error {
            url: parsed.normalized_url,
            error_code: code,
            message: stderr_msg.trim().to_owned(),
        };
    }

    if status_code >= 400 {
        return WebFetchOutcome::Error {
            url: parsed.normalized_url,
            error_code: "url_not_accessible",
            message: format!("HTTP {status_code}"),
        };
    }

    let lower_ct = content_type.to_ascii_lowercase();
    let is_html =
        lower_ct.contains("html") || lower_ct.is_empty() && body_part.trim_start().starts_with('<');

    let (title, cleaned_text, reported_media) = if is_html {
        let extracted_title = extract_html_title(body_part);
        let readable = html_to_readable_text(body_part);
        (extracted_title, readable, "text/plain".to_owned())
    } else {
        let media = content_type
            .split(';')
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("text/plain")
            .to_owned();
        (None, body_part.trim().to_owned(), media)
    };

    let bounded = truncate_utf8(&cleaned_text, MAX_EXTRACTED_CHARS);
    WebFetchOutcome::Success {
        url: parsed.normalized_url,
        title,
        media_type: reported_media,
        content: bounded,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ValidatedUrl {
    normalized_url: String,
    host: String,
}

fn validate_fetch_url(
    raw: &str,
    allowed_domains: &[String],
    blocked_domains: &[String],
) -> Result<ValidatedUrl, (&'static str, String)> {
    let rest = if let Some(r) = raw.strip_prefix("https://") {
        r
    } else if let Some(r) = raw.strip_prefix("http://") {
        r
    } else {
        return Err((
            "invalid_input",
            "only http:// and https:// URLs are supported".to_owned(),
        ));
    };

    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = rest.get(..authority_end).unwrap_or(rest);
    if authority.is_empty() || authority.contains('@') {
        return Err(("invalid_input", "invalid URL authority".to_owned()));
    }

    let host_raw = if let Some(bracketed) = authority.strip_prefix('[') {
        let close = bracketed
            .find(']')
            .ok_or(("invalid_input", "malformed IPv6 host".to_owned()))?;
        bracketed.get(..close).unwrap_or_default()
    } else {
        authority.split_once(':').map_or(authority, |(h, _)| h)
    };

    let host = host_raw.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return Err(("invalid_input", "missing URL host".to_owned()));
    }

    if is_ssrf_forbidden_host(&host) {
        return Err((
            "url_not_allowed",
            "fetching loopback, private, or link-local hosts is not permitted".to_owned(),
        ));
    }

    if blocked_domains
        .iter()
        .any(|d| domain_matches(&host, d.trim()))
    {
        return Err((
            "url_not_allowed",
            format!("domain `{host}` is in blocked_domains"),
        ));
    }

    if !allowed_domains.is_empty()
        && !allowed_domains
            .iter()
            .any(|d| domain_matches(&host, d.trim()))
    {
        return Err((
            "url_not_allowed",
            format!("domain `{host}` is not in allowed_domains"),
        ));
    }

    Ok(ValidatedUrl {
        normalized_url: raw.to_owned(),
        host,
    })
}

fn domain_matches(host: &str, rule: &str) -> bool {
    let clean_rule = rule
        .trim_start_matches("*.")
        .trim_start_matches('.')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if clean_rule.is_empty() {
        return false;
    }
    if host == clean_rule {
        return true;
    }
    host.strip_suffix(&clean_rule)
        .is_some_and(|prefix| prefix.ends_with('.'))
}

fn is_ssrf_forbidden_host(host: &str) -> bool {
    if host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host == "metadata.google.internal"
    {
        return true;
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => is_forbidden_ipv4(v4),
            IpAddr::V6(v6) => is_forbidden_ipv6(v6),
        };
    }

    false
}

const fn is_forbidden_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_unspecified()
        || octets[0] == 0
        || (octets[0] == 100 && (octets[1] & 0b1100_0000) == 0b0100_0000)
}

const fn is_forbidden_ipv6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return true;
    }
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_forbidden_ipv4(v4);
    }
    let seg0 = ip.segments()[0];
    // Unique local (fc00::/7) or link-local (fe80::/10)
    (seg0 & 0xfe00) == 0xfc00 || (seg0 & 0xffc0) == 0xfe80
}

fn split_curl_trailer(stdout: &str) -> (&str, u16, &str) {
    let marker = "\n__AGY_CURL_META__:";
    let Some(pos) = stdout.rfind(marker) else {
        return (stdout, 200, "text/plain");
    };
    let Some((body, rest)) = stdout.split_at_checked(pos) else {
        return (stdout, 200, "text/plain");
    };
    let meta = rest.strip_prefix(marker).unwrap_or_default().trim();
    let (code_s, ct_s) = meta.split_once('\t').unwrap_or((meta, ""));
    let code = code_s.trim().parse::<u16>().unwrap_or(0);
    (body, code, ct_s.trim())
}

fn extract_html_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start_tag = lower.find("<title")?;
    let after_open = html.get(start_tag..)?.find('>')? + start_tag + 1;
    let end_tag_rel = lower.get(after_open..)?.find("</title>")?;
    let raw_title = html.get(after_open..after_open + end_tag_rel)?;
    let decoded = decode_basic_entities(raw_title);
    let collapsed = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    (!collapsed.is_empty()).then_some(collapsed)
}

/// Converts raw HTML into clean, token-efficient readable text without external DOM crates.
#[must_use]
pub fn html_to_readable_text(html: &str) -> String {
    let without_scripts = strip_case_insensitive_block(html, "<script", "</script>");
    let without_styles = strip_case_insensitive_block(&without_scripts, "<style", "</style>");
    let without_svg = strip_case_insensitive_block(&without_styles, "<svg", "</svg>");
    let without_noscript = strip_case_insensitive_block(&without_svg, "<noscript", "</noscript>");

    let mut plain = String::with_capacity(without_noscript.len() / 2);
    let mut in_tag = false;
    let mut tag_name_buf = String::with_capacity(16);
    let mut collecting_tag_name = false;

    for ch in without_noscript.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                let t = tag_name_buf.trim_start_matches('/').to_ascii_lowercase();
                if matches!(
                    t.as_str(),
                    "p" | "div"
                        | "br"
                        | "li"
                        | "tr"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                        | "section"
                        | "article"
                        | "header"
                        | "footer"
                        | "pre"
                        | "blockquote"
                ) && !plain.ends_with('\n')
                {
                    plain.push('\n');
                } else if !plain.ends_with([' ', '\n']) {
                    plain.push(' ');
                }
                tag_name_buf.clear();
                collecting_tag_name = false;
            } else if collecting_tag_name {
                if ch.is_whitespace() {
                    collecting_tag_name = false;
                } else if tag_name_buf.len() < 16 {
                    tag_name_buf.push(ch);
                }
            }
        } else if ch == '<' {
            in_tag = true;
            collecting_tag_name = true;
            tag_name_buf.clear();
        } else {
            plain.push(ch);
        }
    }

    let decoded = decode_basic_entities(&plain);
    let mut normalized = String::with_capacity(decoded.len());
    let mut blank_streak = 0usize;
    for line in decoded.lines() {
        let collapsed_line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed_line.is_empty() {
            blank_streak += 1;
            if blank_streak == 1 && !normalized.is_empty() {
                normalized.push('\n');
            }
        } else {
            blank_streak = 0;
            if !normalized.is_empty() && !normalized.ends_with('\n') {
                normalized.push('\n');
            }
            normalized.push_str(&collapsed_line);
            normalized.push('\n');
        }
    }
    normalized.trim().to_owned()
}

fn strip_case_insensitive_block(input: &str, open_prefix: &str, close_tag: &str) -> String {
    let lower = input.to_ascii_lowercase();
    if !lower.contains(open_prefix) {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0usize;
    while let Some(rel_start) = lower.get(cursor..).and_then(|s| s.find(open_prefix)) {
        let abs_start = cursor + rel_start;
        if let Some(before) = input.get(cursor..abs_start) {
            out.push_str(before);
        }
        let search_from = abs_start + open_prefix.len();
        if let Some(rel_end) = lower.get(search_from..).and_then(|s| s.find(close_tag)) {
            cursor = search_from + rel_end + close_tag.len();
        } else {
            cursor = input.len();
            break;
        }
    }
    if let Some(tail) = input.get(cursor..) {
        out.push_str(tail);
    }
    out
}

fn decode_basic_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_owned();
    }
    input
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

fn truncate_utf8(input: &str, max_chars: usize) -> String {
    let Some((byte_idx, _)) = input.char_indices().nth(max_chars) else {
        return input.to_owned();
    };
    let Some((head, _)) = input.split_at_checked(byte_idx) else {
        return input.to_owned();
    };
    let mut out = head.trim_end().to_owned();
    out.push_str("\n\n[...content truncated...]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_ssrf_and_enforces_domain_policies() {
        assert!(validate_fetch_url("http://127.0.0.1:8787/healthz", &[], &[]).is_err());
        assert!(validate_fetch_url("http://localhost:3000/secret", &[], &[]).is_err());
        assert!(validate_fetch_url("http://169.254.169.254/latest/meta-data", &[], &[]).is_err());
        assert!(validate_fetch_url("http://[::1]:8080/", &[], &[]).is_err());
        assert!(validate_fetch_url("file:///etc/passwd", &[], &[]).is_err());

        let allowed = vec!["rust-lang.org".to_owned()];
        assert!(
            validate_fetch_url("https://doc.rust-lang.org/edition-guide/", &allowed, &[]).is_ok()
        );
        assert!(validate_fetch_url("https://example.com/", &allowed, &[]).is_err());

        let blocked = vec!["evil.example.com".to_owned()];
        assert!(validate_fetch_url("https://sub.evil.example.com/a", &[], &blocked).is_err());
    }

    #[test]
    fn extracts_clean_title_and_readable_text_from_html() {
        let html = r#"
            <html>
              <head>
                <title>Singapore 24-Hr Weather &amp; Forecast</title>
                <style>body { color: red; }</style>
                <script>console.log("ignore");</script>
              </head>
              <body>
                <h1>24-Hour Forecast</h1>
                <p>Temperature: 25&deg;C - 31&deg;C &amp; humid.</p>
              </body>
            </html>
        "#;
        assert_eq!(
            extract_html_title(html).as_deref(),
            Some("Singapore 24-Hr Weather & Forecast")
        );
        let text = html_to_readable_text(html);
        assert!(text.contains("24-Hour Forecast"));
        assert!(text.contains("Temperature:"));
        assert!(!text.contains("console.log"));
        assert!(!text.contains("color: red"));
    }
}
