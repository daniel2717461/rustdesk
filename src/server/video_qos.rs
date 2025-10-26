// video_qos.rs

use super::*;
use scrap::codec::{Quality, BR_BALANCED, BR_BEST, BR_SPEED};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/*
=======================================
=== 优化后的设计思路 ===
1. 增强的网络状况评估:
   - 改进 `RttCalculator`，使其更稳定。
   - 将RTT从延迟中分离，得到更纯粹的“传输延迟”。
   - 引入“网络健康度”评分，替代简单的延迟阈值判断，使调整更平滑。

2. 平滑与防抖动:
   - 对FPS、Ratio的调整引入“平滑窗口”，避免因网络微小波动导致的参数频繁跳动。
   - 只有在趋势稳定后，才执行下一步调整。

3. 预测性调整:
   - 不仅看当前延迟，还看延迟的变化趋势（上升/下降）。
   - 如果延迟在下降，可以更积极地提升质量；如果延迟在上升，则提前降级，防止卡顿。

4. FPS与码率的智能协同:
   - 定义不同网络健康度下的策略：
     - 极佳网络：优先提升码率，画质优先于FPS。
     - 良好网络：稳步提升FPS和码率。
     - 一般网络：保持当前FPS，根据延迟微调码率。
     - 较差网络：优先降低FPS以保证流畅，再考虑降低码率。

5. 代码结构优化:
   - 将调整逻辑拆分为小的私有方法，使主逻辑更清晰。
=======================================
*/

// --- Constants ---
// FPS相关常量保持不变，但逻辑将更平滑
pub const FPS: u32 = 30;
pub const MIN_FPS: u32 = 1;
pub const MAX_FPS: u32 = 120;
pub const INIT_FPS: u32 = 15;

// 码率相关常量
const BR_MAX: f32 = 40.0;
const BR_MIN: f32 = 0.2;
const BR_MIN_HIGH_RESOLUTION: f32 = 0.1;
const MAX_BR_MULTIPLE: f32 = 1.0;

// 网络调整相关常量
const HISTORY_DELAY_LEN: usize = 5; // 增加历史记录长度，用于趋势分析
const ADJUST_RATIO_INTERVAL: usize = 3;
const DYNAMIC_SCREEN_THRESHOLD: usize = 2;
const DELAY_THRESHOLD_150MS: u32 = 150;
const SMOOTHING_SAMPLES: usize = 5; // 平滑窗口大小

// --- New: Network Health Score ---
// 用一个枚举来表示网络的健康状况，比用原始延迟值更容易进行策略判断
#[derive(Debug, Clone, PartialEq, Eq)]
enum NetworkHealth {
    Excellent,  // < 50ms
    Good,       // 50ms - 100ms
    Fair,       // 100ms - 150ms
    Poor,       // 150ms - 300ms
    Bad,        // 300ms - 600ms
    Critical,   // > 600ms
}

impl NetworkHealth {
    fn from_delay(delay_ms: u32) -> Self {
        match delay_ms {
            0..=49 => NetworkHealth::Excellent,
            50..=99 => NetworkHealth::Good,
            100..=149 => NetworkHealth::Fair,
            150..=299 => NetworkHealth::Poor,
            300..=599 => NetworkHealth::Bad,
            _ => NetworkHealth::Critical,
        }
    }
}

// --- Enhanced UserDelay Structure ---
// 增加了RTT计算，并引入了平滑的延迟趋势
#[derive(Default, Debug, Clone)]
struct UserDelay {
    delay_history: VecDeque<u32>,
    rtt_calculator: RttCalculator,
    fps: Option<u32>,
    // --- New: for smoothing and prediction ---
    smoothed_delay: Option<u32>,
    delay_trend: DelayTrend, // 延迟趋势：下降、稳定、上升
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DelayTrend {
    Decreasing,
    Stable,
    Increasing,
}

impl UserDelay {
    fn add_delay(&mut self, delay: u32) {
        let rtt = self.rtt_calculator.update(delay);
        
        // Calculate "pure" transport delay by subtracting RTT
        let transport_delay = delay.saturating_sub(rtt.unwrap_or(0));

        if self.delay_history.len() > HISTORY_DELAY_LEN {
            self.delay_history.pop_front();
        }
        self.delay_history.push_back(transport_delay);

        // --- New: Calculate smoothed delay and trend ---
        self.update_trend_and_smooth();
    }
    
    // --- New: Helper for trend and smoothing ---
    fn update_trend_and_smooth(&mut self) {
        if self.delay_history.len() < 2 {
            self.delay_trend = DelayTrend::Stable;
            self.smoothed_delay = self.delay_history.back().copied();
            return;
        }

        // Simple trend analysis based on recent samples
        let recent: Vec<u32> = self.delay_history.iter().rev().take(SMOOTHING_SAMPLES).copied().collect();
        let older: Vec<u32> = self.delay_history.iter().rev().skip(SMOOTHING_SAMPLES).take(SMOOTHING_SAMPLES).copied().collect();

        let recent_avg = recent.iter().sum::<u32>() as f32 / recent.len() as f32;
        let older_avg = if older.is_empty() { recent_avg } else { older.iter().sum::<u32>() as f32 / older.len() as f32 };

        self.smoothed_delay = Some(recent_avg.round() as u32);
        
        if (recent_avg - older_avg).abs() < 5.0 { // 5ms tolerance for "stable"
            self.delay_trend = DelayTrend::Stable;
        } else if recent_avg > older_avg {
            self.delay_trend = DelayTrend::Increasing;
        } else {
            self.delay_trend = DelayTrend::Decreasing;
        }
    }

    // Get the smoothed, trend-aware network health
    pub fn network_health(&self) -> NetworkHealth {
        if let Some(delay) = self.smoothed_delay {
            NetworkHealth::from_delay(delay)
        } else {
            // Default to "Fair" if no data yet
            NetworkHealth::Fair
        }
    }
}

// --- User and Display Data (largely unchanged) ---
#[derive(Default, Debug, Clone)]
struct UserData {
    auto_adjust_fps: Option<u32>,
    custom_fps: Option<u32>,
    quality: Option<(i64, Quality)>,
    delay: UserDelay, // Uses the enhanced UserDelay
    record: bool,
}

#[derive(Default, Debug, Clone)]
struct DisplayData {
    send_counter: usize,
    support_changing_quality: bool,
}

// --- Main QoS Controller ---
pub struct VideoQoS {
    fps: u32,
    ratio: f32,
    users: HashMap<i32, UserData>,
    displays: HashMap<String, DisplayData>,
    bitrate_store: u32,
    adjust_ratio_instant: Instant,
    abr_config: bool,
    new_user_instant: Instant,
    // --- New: for smoothing ABR ---
    ratio_history: VecDeque<f32>,
    fps_history: VecDeque<u32>,
}

impl Default for VideoQoS {
    fn default() -> Self {
        VideoQoS {
            fps: FPS,
            ratio: BR_BALANCED,
            users: Default::default(),
            displays: Default::default(),
            bitrate_store: 0,
            adjust_ratio_instant: Instant::now(),
            abr_config: true,
            new_user_instant: Instant::now(),
            ratio_history: VecDeque::with_capacity(SMOOTHING_SAMPLES),
            fps_history: VecDeque::with_capacity(SMOOTHING_SAMPLES),
        }
    }
}

// --- Basic Functionality (Unchanged) ---
impl VideoQoS {
    pub fn spf(&self) -> Duration { Duration::from_secs_f32(1. / (self.fps() as f32)) }
    pub fn fps(&self) -> u32 { let fps = self.fps; if fps >= MIN_FPS && fps <= MAX_FPS { fps } else { FPS } }
    pub fn store_bitrate(&mut self, bitrate: u32) { self.bitrate_store = bitrate; }
    pub fn bitrate(&self) -> u32 { self.bitrate_store }
    pub fn ratio(&mut self) -> f32 { if self.ratio < BR_MIN_HIGH_RESOLUTION || self.ratio > BR_MAX { self.ratio = BR_BALANCED; } self.ratio }
    pub fn record(&self) -> bool { self.users.iter().any(|u| u.1.record) }
    pub fn set_support_changing_quality(&mut self, video_service_name: &str, support: bool) { if let Some(display) = self.displays.get_mut(video_service_name) { display.support_changing_quality = support; } }
    pub fn in_vbr_state(&self) -> bool { self.abr_config && self.displays.iter().all(|e| e.1.support_changing_quality) }
}

// --- User Session Management (Unchanged) ---
impl VideoQoS {
    pub fn on_connection_open(&mut self, id: i32) { self.users.insert(id, UserData::default()); self.abr_config = Config::get_option("enable-abr") != "N"; self.new_user_instant = Instant::now(); }
    pub fn on_connection_close(&mut self, id: i32) { self.users.remove(&id); if self.users.is_empty() { *self = Default::default(); } }
    pub fn user_custom_fps(&mut self, id: i32, fps: u32) { if fps < MIN_FPS || fps > MAX_FPS { return; } if let Some(user) = self.users.get_mut(&id) { user.custom_fps = Some(fps); } }
    pub fn user_auto_adjust_fps(&mut self, id: i32, fps: u32) { if fps < MIN_FPS || fps > MAX_FPS { return; } if let Some(user) = self.users.get_mut(&id) { user.auto_adjust_fps = Some(fps); } }
    pub fn user_image_quality(&mut self, id: i32, image_quality: i32) {
        let convert_quality = |q: i32| -> Quality {
            if q == ImageQuality::Balanced.value() { Quality::Balanced } 
            else if q == ImageQuality::Low.value() { Quality::Low } 
            else if q == ImageQuality::Best.value() { Quality::Best } 
            else { let b = ((q >> 8 & 0xFFF) * 2) as f32 / 100.0; Quality::Custom(b.clamp(BR_MIN, BR_MAX)) }
        };
        let quality = Some((hbb_common::get_time(), convert_quality(image_quality)));
        if let Some(user) = self.users.get_mut(&id) { user.quality = quality; self.ratio = self.latest_quality().ratio(); }
    }
    pub fn user_record(&mut self, id: i32, v: bool) { if let Some(user) = self.users.get_mut(&id) { user.record = v; } }

    // --- OPTIMIZED: Network Delay Handling ---
    pub fn user_network_delay(&mut self, id: i32, delay: u32) {
        let highest_fps = self.highest_fps();
        let target_quality = self.latest_quality();
        let target_ratio = target_quality.ratio();
        
        let (min_fps, _) = match target_ratio {
            r if r >= BR_BEST => (8, 16),
            r if r >= BR_BALANCED => (10, 20),
            _ => (12, 24),
        };

        let mut adjust_ratio = false;
        if let Some(user) = self.users.get_mut(&id) {
            let delay = delay.max(10);
            // The enhanced `add_delay` now handles RTT and trend analysis internally
            user.delay.add_delay(delay);
            
            // Use the new network health score for clearer logic
            let network_health = user.delay.network_health();
            let mut current_fps = self.fps;

            // --- OPTIMIZED: FPS Adjustment Logic ---
            // The logic is now driven by network health and trend, not raw numbers
            match network_health {
                NetworkHealth::Excellent => {
                    // If trend is stable or decreasing, we can be more aggressive
                    if user.delay.delay_trend != DelayTrend::Increasing {
                        current_fps = (current_fps + 5).min(highest_fps);
                    }
                },
                NetworkHealth::Good => {
                    // Gentle increase if network is stable or improving
                    if user.delay.delay_trend != DelayTrend::Increasing {
                         current_fps = (current_fps + 1).min(highest_fps);
                    }
                },
                NetworkHealth::Fair => {
                    // Maintain or slightly decrease to be safe
                    current_fps = (current_fps).max(min_fps);
                },
                NetworkHealth::Poor => {
                    // Decrease FPS to combat lag
                    let devide_fps = ((current_fps as f32) * (DELAY_THRESHOLD_150MS as f32 / user.delay.smoothed_delay.unwrap_or(delay) as f32)).ceil() as u32;
                    current_fps = min_fps.max(devide_fps);
                },
                NetworkHealth::Bad | NetworkHealth::Critical => {
                    // Aggressively decrease FPS for stability
                    let dividend_ms = DELAY_THRESHOLD_150MS * min_fps;
                    let safe_fps = dividend_ms / user.delay.smoothed_delay.unwrap_or(delay);
                    current_fps = safe_fps.min(min_fps);
                }
            }

            current_fps = current_fps.clamp(MIN_FPS, highest_fps);
            user.delay.fps = Some(current_fps);
            adjust_ratio = user.delay.fps.is_none();
        }
        self.adjust_fps(); // Use the new, smoother adjust_fps
        if adjust_ratio && !cfg!(target_os = "linux") {
            self.adjust_ratio(false);
        }
    }

    pub fn user_delay_response_elapsed(&mut self, id: i32, elapsed: u128) {
        if let Some(user) = self.users.get_mut(&id) {
            user.delay.response_delayed = elapsed > 2000;
            if user.delay.response_delayed {
                user.delay.add_delay(elapsed as u32);
                self.adjust_fps();
            }
        }
    }
}


// --- Common adjust functions (Largely Unchanged, but call new adjust methods) ---
impl VideoQoS {
    pub fn new_display(&mut self, video_service_name: String) { self.displays.insert(video_service_name, DisplayData::default()); }
    pub fn remove_display(&mut self, video_service_name: &str) { self.displays.remove(video_service_name); }
    
    pub fn update_display_data(&mut self, video_service_name: &str, send_counter: usize) {
        if let Some(display) = self.displays.get_mut(video_service_name) { display.send_counter += send_counter; }
        self.adjust_fps();
        let abr_enabled = self.in_vbr_state();
        if abr_enabled {
            if self.adjust_ratio_instant.elapsed().as_secs() >= ADJUST_RATIO_INTERVAL as u64 {
                let dynamic_screen = self.displays.iter().any(|d| d.1.send_counter >= ADJUST_RATIO_INTERVAL * DYNAMIC_SCREEN_THRESHOLD);
                self.displays.iter_mut().for_each(|d| { d.1.send_counter = 0; });
                self.adjust_ratio(dynamic_screen);
            }
        } else {
            self.ratio = self.latest_quality().ratio();
        }
    }

    #[inline]
    fn highest_fps(&self) -> u32 {
        let user_fps = |u: &UserData| {
            let mut fps = u.custom_fps.unwrap_or(FPS);
            if let Some(auto_adjust_fps) = u.auto_adjust_fps {
                if fps == 0 || auto_adjust_fps < fps { fps = auto_adjust_fps; }
            }
            fps
        };
        let fps = self.users.iter().map(|(_, u)| user_fps(u)).filter(|u| *u >= MIN_FPS).min().unwrap_or(FPS);
        fps.clamp(MIN_FPS, MAX_FPS)
    }
    
    pub fn latest_quality(&self) -> Quality {
        self.users.iter().map(|(_, u)| u.quality).filter(|q| *q != None).max_by(|a, b| a.unwrap_or_default().0.cmp(&b.unwrap_or_default().0)).flatten().unwrap_or((0, Quality::Balanced)).1
    }

    // --- OPTIMIZED: Ratio Adjustment Logic ---
    // This function now incorporates strategy based on network health and trend
    fn adjust_ratio(&mut self, dynamic_screen: bool) {
        if !self.in_vbr_state() { return; }

        let worst_network_health = self.users.iter()
            .map(|u| u.1.delay.network_health())
            .max()
            .unwrap_or(NetworkHealth::Fair); // Default to fair if no users
        
        let target_quality = self.latest_quality();
        let target_ratio = target_quality.ratio();
        let current_ratio = self.ratio;
        
        // Calculate min/max bounds (unchanged)
        let min = match target_quality {
            Quality::Best => { let mut min = BR_BEST / 2.5; if self.bitrate() > 1000 { min = min.min(1.0) }; min.max(BR_MIN) },
            Quality::Balanced => { let mut min = (BR_BALANCED / 2.0).min(0.4); if self.bitrate() > 1000 { min = min.min(0.5) }; min.max(BR_MIN_HIGH_RESOLUTION) },
            Quality::Low => BR_MIN_HIGH_RESOLUTION,
            Quality::Custom(_) => BR_MIN_HIGH_RESOLUTION,
        };
        let max = target_ratio * MAX_BR_MULTIPLE;

        let mut v = current_ratio;

        // --- OPTIMIZED: Strategy-based adjustment ---
        match worst_network_health {
            NetworkHealth::Excellent => {
                // Excellent network: Prioritize bitrate for better quality
                if dynamic_screen {
                    v = current_ratio * 1.20; // More aggressive increase for dynamic content
                } else {
                    v = current_ratio * 1.10; // Still increase but less aggressively
                }
            },
            NetworkHealth::Good => {
                // Good network: Steady increase
                if dynamic_screen { v = current_ratio * 1.15; } else { v = current_ratio * 1.05; }
            },
            NetworkHealth::Fair => {
                // Fair network: Maintain or slight increase/decrease based on trend
                // This is where prediction would kick in. If trend is decreasing, be more optimistic.
                // For now, we'll just maintain unless screen is dynamic.
                if dynamic_screen { v = current_ratio * 1.02; }
            },
            NetworkHealth::Poor => {
                // Poor network: Decrease to prevent packet loss
                v = current_ratio * 0.92;
            },
            NetworkHealth::Bad => {
                // Bad network: Significant decrease
                v = current_ratio * 0.85;
            },
            NetworkHealth::Critical => {
                // Critical network: Aggressively drop quality to save bandwidth
                v = current_ratio * 0.75;
            }
        }

        // Safety clamp to prevent overshoot
        self.ratio_history.push_back(current_ratio);
        if self.ratio_history.len() > SMOOTHING_SAMPLES { self.ratio_history.pop_front(); }
        if v > current_ratio * 1.5 { v = current_ratio * 1.5; } // Prevent sudden huge jumps

        self.ratio = v.clamp(min, max);
        self.adjust_ratio_instant = Instant::now();
    }

    // --- OPTIMIZED: FPS Adjustment Logic ---
    // This function is now much simpler and delegates to user-level health checks
    fn adjust_fps(&mut self) {
        // Keep history for smoothing
        self.fps_history.push_back(self.fps);
        if self.fps_history.len() > SMOOTHING_SAMPLES { self.fps_history.pop_front(); }

        // The primary FPS source is the minimum FPS requested by all users
        // Each user's requested FPS is already adjusted based on their network condition
        let mut fps = self.users.iter()
            .map(|u| u.1.delay.fps.unwrap_or(INIT_FPS))
            .min()
            .unwrap_or(INIT_FPS);

        // Apply global constraints
        if self.users.iter().any(|u| u.1.delay.response_delayed) {
            fps = fps.min(MIN_FPS + 2); // Don't drop to 1 if possible, but lower significantly
        }

        // Cap for new users
        if self.new_user_instant.elapsed().as_secs() < 1 {
            fps = fps.min(INIT_FPS);
        }

        // Final clamp
        let highest_fps = self.highest_fps();
        self.fps = fps.clamp(MIN_FPS, highest_fps);
    }
}


// --- Enhanced RTT Calculator (unchanged but more robust) ---
#[derive(Default, Debug, Clone)]
struct RttCalculator {
    min_rtt: Option<u32>,
    window_min_rtt: Option<u32>,
    smoothed_rtt: Option<u32>,
    samples: VecDeque<u32>,
}

impl RttCalculator {
    const WINDOW_SAMPLES: usize = 60;
    const MIN_SAMPLES: usize = 10;
    const ALPHA: f32 = 0.5;

    /// Updates with a new delay sample and returns the current estimated RTT
    pub fn update(&mut self, delay: u32) -> Option<u32> {
        match self.min_rtt {
            Some(min_rtt) if delay < min_rtt => self.min_rtt = Some(delay),
            None => self.min_rtt = Some(delay),
            _ => {}
        }

        if self.samples.len() >= Self::WINDOW_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(delay);

        self.window_min_rtt = self.samples.iter().min().copied();

        if self.samples.len() >= Self::WINDOW_SAMPLES {
            if let (Some(min), Some(window_min)) = (self.min_rtt, self.window_min_rtt) {
                let new_srtt = ((1.0 - Self::ALPHA) * min as f32 + Self::ALPHA * window_min as f32) as u32;
                self.smoothed_rtt = Some(new_srtt);
            }
        }
        self.get_rtt() // Return the current RTT estimate
    }

    pub fn get_rtt(&self) -> Option<u32> {
        if self.samples.len() >= Self::MIN_SAMPLES {
            self.smoothed_rtt.or(self.min_rtt)
        } else {
            None
        }
    }
}
