//! egui dashboard for `pc_sink`.
//!
//! Renders three stacked time-series plots — currents, temperature, humidity —
//! sharing one absolute-epoch-ms X-axis. On every [`DrainEvent`] (and at
//! startup) the cache is refreshed from an **independent read-only**
//! [`SessionStore`] handle, so the UI never touches the acquisition writer's
//! mutex. The pure data-shaping helpers ([`window_cutoff_ms`],
//! [`series_points`]) are split out from the egui glue so they can be unit
//! tested without a window or a clock.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use egui_plot::{Legend, Line, Plot};
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::TryRecvError;

use pc_sink::ble::DrainEvent;
use pc_sink::models::Sample;
use pc_sink::store::{SessionStore, TagId};

/// Default trailing window shown when "show all" is off.
const DEFAULT_WINDOW_MINUTES: u32 = 5;

/// Milliseconds in one minute, for the trailing-window cutoff.
const MS_PER_MINUTE: i64 = 60_000;

/// Repaint cadence so the UI stays live between input/drain events.
const REPAINT_INTERVAL: Duration = Duration::from_millis(250);

/// The dashboard application state.
pub struct App {
    /// Drain notifications from the acquisition loop; each one triggers a refresh.
    events: Receiver<DrainEvent>,
    /// Independent read-only connection to the session DB (never the writer mutex).
    store: SessionStore,
    /// Cached samples per tag, refreshed on each drain event.
    series: Vec<(TagId, Vec<Sample>)>,
    /// Trailing window in minutes (ignored when `show_all`).
    window_minutes: u32,
    /// When true, plot the full history instead of the trailing window.
    show_all: bool,
}

impl App {
    /// Builds the dashboard from the drain-event receiver and a read-only store.
    ///
    /// Performs an initial [`refresh`](Self::refresh) so any already-stored
    /// samples are shown before the first drain event arrives.
    pub fn new(events: Receiver<DrainEvent>, store: SessionStore) -> Self {
        let mut app = Self {
            events,
            store,
            series: Vec::new(),
            window_minutes: DEFAULT_WINDOW_MINUTES,
            show_all: false,
        };
        app.refresh();
        app
    }

    /// Drains all queued drain events; returns true if at least one arrived.
    ///
    /// Non-blocking: `Empty` stops the loop, `Lagged` is logged and counts as an
    /// event (the cache should still refresh), and `Closed` stops polling.
    fn poll_events(&mut self) -> bool {
        let mut received = false;
        loop {
            match self.events.try_recv() {
                Ok(event) => {
                    log::debug!(
                        "drain event: tag {} ({} samples)",
                        event.tag_id,
                        event.samples_stored
                    );
                    received = true;
                }
                Err(TryRecvError::Lagged(skipped)) => {
                    log::warn!("UI drain-event subscriber lagged; skipped {skipped} events");
                    received = true;
                }
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        received
    }

    /// Reloads `series` from the store for the current window setting.
    ///
    /// The trailing-window cutoff is computed here in the app layer (the store
    /// stays clock-free). A per-tag query error is logged and that tag dropped,
    /// not fatal.
    fn refresh(&mut self) {
        let cutoff = window_cutoff_ms(now_epoch_ms(), self.window_minutes, self.show_all);
        let tags = match self.store.tag_ids() {
            Ok(tags) => tags,
            Err(error) => {
                log::error!("listing tags failed: {error}");
                return;
            }
        };
        self.series = tags
            .into_iter()
            .filter_map(|tag| {
                let result = match cutoff {
                    Some(since_ms) => self.store.samples_since(&tag, since_ms),
                    None => self.store.samples_for(&tag),
                };
                match result {
                    Ok(samples) => {
                        log::debug!("refreshed tag {tag}: {} samples", samples.len());
                        Some((tag, samples))
                    }
                    Err(error) => {
                        log::error!("loading samples for {tag} failed: {error}");
                        None
                    }
                }
            })
            .collect();
    }
}

impl eframe::App for App {
    /// Polls drain events, draws the controls and the three plots, and schedules
    /// the next repaint.
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut needs_refresh = self.poll_events();

        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Trailing window (minutes):");
                let window_changed = ui
                    .add_enabled(
                        !self.show_all,
                        egui::DragValue::new(&mut self.window_minutes).range(1..=1440),
                    )
                    .changed();
                let show_all_changed = ui.checkbox(&mut self.show_all, "Show all").changed();
                if window_changed || show_all_changed {
                    needs_refresh = true;
                }
            });
        });

        if needs_refresh {
            self.refresh();
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // Divide the panel evenly among the three stacked plots up front, so
            // later plots aren't squeezed by the height the earlier ones took.
            let plot_height = ui.available_height() / 3.0;

            Plot::new("currents")
                .legend(Legend::default())
                .height(plot_height)
                .show(ui, |plot_ui| {
                    for (tag, samples) in &self.series {
                        plot_ui.line(Line::new(
                            format!("{tag} A"),
                            series_points(samples, |sample| sample.current_a),
                        ));
                        plot_ui.line(Line::new(
                            format!("{tag} B"),
                            series_points(samples, |sample| sample.current_b),
                        ));
                    }
                });

            Plot::new("temperature")
                .legend(Legend::default())
                .height(plot_height)
                .show(ui, |plot_ui| {
                    for (tag, samples) in &self.series {
                        plot_ui.line(Line::new(
                            tag.to_string(),
                            series_points(samples, |sample| sample.temperature_c),
                        ));
                    }
                });

            Plot::new("humidity")
                .legend(Legend::default())
                .height(plot_height)
                .show(ui, |plot_ui| {
                    for (tag, samples) in &self.series {
                        plot_ui.line(Line::new(
                            tag.to_string(),
                            series_points(samples, |sample| sample.humidity_pct),
                        ));
                    }
                });
        });

        ctx.request_repaint_after(REPAINT_INTERVAL);
    }
}

/// Current wall-clock epoch ms, for computing the trailing-window cutoff.
fn now_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// Computes the query cutoff for the trailing window.
///
/// Returns `None` when `show_all` is set (no lower bound), otherwise
/// `now_ms - window_minutes * 60_000`.
fn window_cutoff_ms(now_ms: i64, window_minutes: u32, show_all: bool) -> Option<i64> {
    if show_all {
        None
    } else {
        Some(now_ms - i64::from(window_minutes) * MS_PER_MINUTE)
    }
}

/// Maps samples to `[x, y]` plot points, with `x` the absolute epoch ms and `y`
/// the value selected by `field`.
fn series_points(samples: &[Sample], field: fn(&Sample) -> f64) -> Vec<[f64; 2]> {
    samples
        .iter()
        .map(|sample| [sample.timestamp_ms as f64, field(sample)])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pc_sink::models::{BlePacket, PACKET_SIZE, SAMPLES_PER_PACKET};

    const TEST_TIME_MS: i64 = 1_700_000_000_000;
    const TEST_DT_MS: u16 = 1_000;

    /// Decodes a known 100-byte packet into its 10 samples.
    ///
    /// Built through the real decoder (rather than a `Sample` struct literal) so
    /// the test does not name the channels this UI deliberately drops.
    fn decoded_samples() -> Vec<Sample> {
        let mut bytes = vec![0u8; PACKET_SIZE];
        // time (i64 LE) at offset 0.
        bytes[0..8].copy_from_slice(&TEST_TIME_MS.to_le_bytes());
        // dt (u16 LE) at offset 88.
        bytes[88..90].copy_from_slice(&TEST_DT_MS.to_le_bytes());
        for index in 0..SAMPLES_PER_PACKET {
            let base = 8 + index * 8;
            // temperature, then humidity, then the two raw ADC channels.
            bytes[base..base + 2].copy_from_slice(&(2_567u16).to_le_bytes());
            bytes[base + 2..base + 4].copy_from_slice(&(4_050u16).to_le_bytes());
            bytes[base + 4..base + 6].copy_from_slice(&(100i16).to_le_bytes());
            bytes[base + 6..base + 8].copy_from_slice(&(-50i16).to_le_bytes());
            // stimulus byte: both nibbles 0x8 (304 mV).
            bytes[90 + index] = 0x88;
        }
        BlePacket::from_bytes(&bytes)
            .expect("known packet decodes")
            .samples()
            .to_vec()
    }

    #[test]
    fn cutoff_subtracts_window_from_now() {
        assert_eq!(window_cutoff_ms(600_000, 5, false), Some(300_000));
    }

    #[test]
    fn cutoff_window_of_one_minute() {
        assert_eq!(window_cutoff_ms(600_000, 1, false), Some(540_000));
    }

    #[test]
    fn show_all_ignores_window() {
        assert_eq!(window_cutoff_ms(600_000, 5, true), None);
    }

    #[test]
    fn maps_current_a_with_derived_timestamp_x() {
        let samples = decoded_samples();
        let points = series_points(&samples, |sample| sample.current_a);
        assert_eq!(points.len(), SAMPLES_PER_PACKET);
        for (index, point) in points.iter().enumerate() {
            // X is the per-sample wall-clock: time + i * dt.
            let expected_x = (TEST_TIME_MS + index as i64 * i64::from(TEST_DT_MS)) as f64;
            assert_eq!(point[0], expected_x);
            assert_eq!(point[1], samples[index].current_a);
        }
    }

    #[test]
    fn maps_current_b_to_points() {
        let samples = decoded_samples();
        let points = series_points(&samples, |sample| sample.current_b);
        assert_eq!(points[0][0], TEST_TIME_MS as f64);
        assert_eq!(
            point_y(&points),
            value_of(&samples, |sample| sample.current_b)
        );
    }

    #[test]
    fn maps_temperature_to_points() {
        let samples = decoded_samples();
        let points = series_points(&samples, |sample| sample.temperature_c);
        assert_eq!(points[0][0], TEST_TIME_MS as f64);
        assert_eq!(
            point_y(&points),
            value_of(&samples, |sample| sample.temperature_c)
        );
    }

    #[test]
    fn maps_humidity_to_points() {
        let samples = decoded_samples();
        let points = series_points(&samples, |sample| sample.humidity_pct);
        assert_eq!(points[0][0], TEST_TIME_MS as f64);
        assert_eq!(
            point_y(&points),
            value_of(&samples, |sample| sample.humidity_pct)
        );
    }

    #[test]
    fn maps_empty_slice_to_no_points() {
        let points = series_points(&[], |sample| sample.current_a);
        assert!(points.is_empty());
    }

    /// Collects the Y values out of plot points.
    fn point_y(points: &[[f64; 2]]) -> Vec<f64> {
        points.iter().map(|point| point[1]).collect()
    }

    /// Collects the selected field value out of samples.
    fn value_of(samples: &[Sample], field: fn(&Sample) -> f64) -> Vec<f64> {
        samples.iter().map(field).collect()
    }
}
