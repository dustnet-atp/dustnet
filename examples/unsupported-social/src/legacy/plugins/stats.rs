use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::super::{FormData, PagePlugin};

/// Shared server statistics, updated by the server and read by the plugin.
pub(crate) struct ServerStats {
    pub active_connections: Arc<AtomicUsize>,
    pub started_at: Instant,
}

/// Stats plugin. Renders server stats into AML.
///
/// Marker: `{{stats}}`
pub(crate) struct StatsPlugin {
    pub stats: Arc<ServerStats>,
}

impl StatsPlugin {
    pub(crate) fn new(stats: Arc<ServerStats>) -> Self {
        StatsPlugin { stats }
    }
}

impl PagePlugin for StatsPlugin {
    fn marker(&self) -> &str {
        "{{stats}}"
    }

    fn render(
        &mut self,
        _aml_path: &Path,
        _query: Option<&str>,
        _peer: SocketAddr,
        _param: Option<&str>,
        _site_root: &Path,
        _identity: Option<&str>,
    ) -> String {
        let connections = self.stats.active_connections.load(Ordering::Relaxed);
        let uptime = self.stats.started_at.elapsed();
        let uptime_str = format_duration(uptime);
        let clock = format_utc_clock();

        let mut out = String::new();
        out.push_str(&stat_row("clients:", &connections.to_string()));
        out.push_str(&stat_row("uptime:", &uptime_str));
        out.push_str(&stat_row("clock:", &clock));
        out
    }

    fn polls(&self) -> bool {
        true
    }

    fn handle_input(
        &mut self,
        _aml_path: &Path,
        _fields: &FormData,
        _query: Option<&str>,
        _identity: Option<&str>,
    ) -> Result<bool, String> {
        Ok(false)
    }
}

fn stat_row(label: &str, value: &str) -> String {
    format!(
        "[row gap=1][col w=8][text fg=white bold align=left]{label}[/text][/col][col][text fg=bright-green align=left]{value}[/text][/col][/row]\n"
    )
}

fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    let s = secs % 60;

    if days > 0 {
        format!("{days}d {hours}h {mins}m")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else if mins > 0 {
        format!("{mins}m {s}s")
    } else {
        format!("{s}s")
    }
}

fn format_utc_clock() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let day_secs = (secs % 86400) as u32;
    let h = day_secs / 3600;
    let m = (day_secs % 3600) / 60;
    let s = day_secs % 60;
    format!("{h:02}:{m:02}:{s:02} UTC")
}

#[cfg(test)]
mod tests {
    use super::stat_row;

    #[test]
    fn stat_rows_separate_labels_and_values() {
        let row = stat_row("clients:", "1");
        assert!(row.starts_with("[row gap=1][col w=8]"));
    }
}
