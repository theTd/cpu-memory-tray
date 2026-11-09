use std::{collections::VecDeque, time::Duration};

pub const SIZE: usize = 32;
pub const MAX_WINDOW: Duration = Duration::from_secs(3600);
// One hour at the smallest supported interval (500 ms), including both endpoints.
pub const MAX_SAMPLES: usize = 7201;

fn usage_color(value: f32) -> [u8; 4] {
    if value >= 80.0 {
        [255, 85, 85, 255]
    } else if value >= 50.0 {
        [255, 205, 65, 255]
    } else {
        [85, 225, 125, 255]
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub at: Duration,
    pub cpu: f32,
    pub memory: f32,
}

pub struct History {
    samples: VecDeque<Sample>,
}

impl History {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(MAX_SAMPLES),
        }
    }

    pub fn push(&mut self, sample: Sample) {
        while self.samples.front().is_some_and(|old| {
            sample.at.saturating_sub(old.at) > MAX_WINDOW || self.samples.len() >= MAX_SAMPLES
        }) {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    // Bucket by real elapsed time, not sample index. Changing the sampling interval
    // therefore never stretches old history. Peaks survive downsampling.
    fn columns(&self, now: Duration, window: Duration) -> [Option<(f32, f32)>; SIZE] {
        let mut columns: [Option<(f32, f32)>; SIZE] = [None; SIZE];
        for sample in &self.samples {
            let Some(age) = now.checked_sub(sample.at) else {
                continue;
            };
            if age > window {
                continue;
            }
            let from_right =
                (age.as_secs_f64() / window.as_secs_f64() * SIZE as f64).floor() as usize;
            let x = SIZE - 1 - from_right.min(SIZE - 1);
            let cpu = sample.cpu.clamp(0.0, 100.0);
            let memory = sample.memory.clamp(0.0, 100.0);
            columns[x] = Some(match columns[x] {
                Some((old_cpu, old_memory)) => (cpu.max(old_cpu), memory.max(old_memory)),
                None => (cpu, memory),
            });
        }
        columns
    }

    pub fn rgba(&self, now: Duration, window: Duration) -> Vec<u8> {
        let mut pixels = vec![0; SIZE * SIZE * 4];
        // Opaque dark background stays readable on both taskbar themes.
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&[18, 23, 30, 255]);
        }
        let mut put = |x: usize, y: usize, color: [u8; 4]| {
            let offset = (y * SIZE + x) * 4;
            pixels[offset..offset + 4].copy_from_slice(&color);
        };
        for x in 0..SIZE {
            put(x, 15, [75, 85, 95, 255]);
            put(x, 31, [75, 85, 95, 255]);
        }
        for (x, column) in self.columns(now, window).iter().enumerate() {
            if let Some((cpu, memory)) = column {
                for (value, bottom) in [(*cpu, 14), (*memory, 30)] {
                    let color = usage_color(value);
                    let height = ((value / 100.0 * 14.0).round() as usize).max(1);
                    for dy in 0..height {
                        put(x, bottom - dy, color);
                    }
                }
            }
        }
        pixels
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn historical_columns_keep_their_severity_colors() {
        let mut history = History::new();
        for (ms, cpu) in [(0, 49.9), (1000, 50.0), (2000, 80.0)] {
            history.push(sample(ms, cpu));
        }
        let rgba = history.rgba(Duration::from_secs(2), Duration::from_secs(32));
        for (x, color) in [
            (29, [85, 225, 125, 255]),
            (30, [255, 205, 65, 255]),
            (31, [255, 85, 85, 255]),
        ] {
            let offset = (14 * SIZE + x) * 4;
            assert_eq!(rgba[offset..offset + 4], color);
        }
    }

    fn sample(ms: u64, cpu: f32) -> Sample {
        Sample {
            at: Duration::from_millis(ms),
            cpu,
            memory: 50.0,
        }
    }

    #[test]
    fn history_is_bounded_and_expires_after_a_long_pause() {
        let mut history = History::new();
        for i in 0..20_000 {
            history.push(sample(i * 500, 20.0));
        }
        assert_eq!(history.samples.len(), MAX_SAMPLES);
        assert_eq!(history.samples.capacity(), MAX_SAMPLES);
        history.push(sample(20_000_000, 30.0));
        assert_eq!(history.samples.len(), 1);
    }

    #[test]
    fn interval_changes_preserve_time_positions_and_peaks() {
        let mut history = History::new();
        history.push(sample(0, 10.0));
        history.push(sample(500, 95.0));
        history.push(sample(1000, 20.0));
        history.push(sample(10_000, 40.0));
        let columns = history.columns(Duration::from_secs(10), Duration::from_secs(32));
        assert_eq!(columns[21], Some((10.0, 50.0)));
        assert_eq!(columns[22], Some((95.0, 50.0)));
        assert_eq!(columns[31], Some((40.0, 50.0)));
        assert_eq!(columns[30], None);
    }

    #[test]
    fn window_excludes_old_and_future_data_without_stretching() {
        let mut history = History::new();
        history.push(sample(0, 100.0));
        history.push(sample(60_000, 50.0));
        history.push(sample(61_000, 90.0));
        let columns = history.columns(Duration::from_secs(60), Duration::from_secs(10));
        assert_eq!(columns.iter().flatten().count(), 1);
        assert_eq!(columns[31], Some((50.0, 50.0)));
        let rgba = history.rgba(Duration::from_secs(60), Duration::from_secs(10));
        assert_eq!(rgba.len(), SIZE * SIZE * 4);
        assert!(rgba.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 255));
    }
}
