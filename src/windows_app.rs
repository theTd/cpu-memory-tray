use crate::graph::{History, SIZE, Sample};
use crate::startup;
use std::{
    error::Error,
    io, mem, ptr,
    time::{Duration, Instant},
};
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, FILETIME, GetLastError, HANDLE},
    System::{
        Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX},
        Threading::{
            CreateMutexW, GetCurrentProcess, GetCurrentThreadId, GetGuiResources,
            GetProcessHandleCount, GetSystemTimes,
        },
    },
    UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, KillTimer, MB_ICONERROR, MB_OK, MSG, MessageBoxW,
        PM_NOREMOVE, PeekMessageW, PostThreadMessageW, RegisterWindowMessageW, SendMessageW,
        SetTimer, TranslateMessage, WM_APP, WM_TIMER,
    },
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const COMMAND: u32 = WM_APP + 1;
const WINDOWS: &[u64] = &[10, 30, 60, 300, 900, 3600];
const INTERVALS: &[u32] = &[500, 1000, 2000, 5000, 10000];

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

pub fn show_error(message: &str) {
    // SAFETY: Both strings are NUL-terminated and live through the synchronous call.
    unsafe {
        MessageBoxW(
            ptr::null_mut(),
            wide(message).as_ptr(),
            wide("CPU / 内存监控").as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

struct OwnedHandle(HANDLE);

struct ComApartment;
impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: Paired with successful CoInitializeEx on the same UI thread.
        unsafe {
            CoUninitialize();
        }
    }
}
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: Only successfully created, owned handles enter this wrapper.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct Timer(usize);
impl Timer {
    fn new(interval: u32) -> Result<Self> {
        // SAFETY: Thread timer; its messages are consumed by the creating thread.
        let id = unsafe { SetTimer(ptr::null_mut(), 0, interval, None) };
        if id == 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self(id))
    }
}
impl Drop for Timer {
    fn drop(&mut self) {
        // SAFETY: Drop runs on the timer's owning thread, including early returns.
        unsafe {
            KillTimer(ptr::null_mut(), self.0);
        }
    }
}

#[derive(Clone, Copy)]
struct CpuTimes {
    idle: u64,
    total: u64,
}
impl CpuTimes {
    fn read() -> Result<Self> {
        let (mut idle, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: All three output pointers refer to initialized writable FILETIMEs.
        if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let value = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
        Ok(Self {
            idle: value(idle),
            total: value(kernel).saturating_add(value(user)),
        })
    }

    fn usage_since(self, previous: Self) -> Option<f32> {
        let total = self.total.checked_sub(previous.total)?;
        let idle = self.idle.checked_sub(previous.idle)?;
        if total == 0 {
            return None;
        }
        Some((100.0 * total.saturating_sub(idle) as f64 / total as f64) as f32)
    }
}

fn memory() -> Result<(u64, u64)> {
    let mut status = MEMORYSTATUSEX {
        dwLength: mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: dwLength is set to the size of the initialized output structure.
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    if status.ullTotalPhys == 0 {
        return Err("Windows reported zero physical memory".into());
    }
    Ok((
        status.ullTotalPhys.saturating_sub(status.ullAvailPhys),
        status.ullTotalPhys,
    ))
}

struct App {
    // Tray/menu wrappers own and destroy their native icons, windows and menus.
    tray: TrayIcon,
    window_items: Vec<CheckMenuItem>,
    interval_items: Vec<CheckMenuItem>,
    startup_item: CheckMenuItem,
    startup_enabled: bool,
    history: History,
    started: Instant,
    previous_cpu: CpuTimes,
    latest: Option<(f32, u64, u64)>,
    window: Duration,
    interval: u32,
}

impl App {
    fn new() -> Result<Self> {
        let menu = Menu::new();
        menu.append(&MenuItem::new(
            "上：CPU  下：内存 · 绿<50% 黄<80% 红≥80%",
            false,
            None,
        ))?;
        let window_menu = Submenu::new("时间窗口", true);
        let mut window_items = Vec::new();
        for seconds in WINDOWS {
            let label = if *seconds < 60 {
                format!("{seconds} 秒")
            } else {
                format!("{} 分钟", seconds / 60)
            };
            let item = CheckMenuItem::with_id(
                format!("window:{seconds}"),
                label,
                true,
                *seconds == 60,
                None,
            );
            window_menu.append(&item)?;
            window_items.push(item);
        }
        menu.append(&window_menu)?;
        let interval_menu = Submenu::new("采样间隔", true);
        let mut interval_items = Vec::new();
        for ms in INTERVALS {
            let item = CheckMenuItem::with_id(
                format!("interval:{ms}"),
                format!("{} 秒", f64::from(*ms) / 1000.0),
                true,
                *ms == 1000,
                None,
            );
            interval_menu.append(&item)?;
            interval_items.push(item);
        }
        menu.append(&interval_menu)?;
        menu.append(&PredefinedMenuItem::separator())?;
        let startup_state = startup::enabled();
        let startup_enabled = startup_state.as_ref().copied().unwrap_or(false);
        let startup_item = CheckMenuItem::with_id(
            "startup",
            if startup_state.is_ok() {
                "开机自启"
            } else {
                "开机自启（无法读取）"
            },
            startup_state.is_ok(),
            startup_enabled,
            None,
        );
        menu.append(&startup_item)?;
        menu.append(&MenuItem::with_id("quit", "退出", true, None))?;

        let history = History::new();
        let icon = Icon::from_rgba(
            history.rgba(Duration::ZERO, Duration::from_secs(60)),
            SIZE as u32,
            SIZE as u32,
        )?;
        let tray = TrayIconBuilder::new()
            .with_icon(icon)
            .with_tooltip("CPU / 内存监控：正在采样…")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()?;
        Ok(Self {
            tray,
            window_items,
            interval_items,
            startup_item,
            startup_enabled,
            history,
            started: Instant::now(),
            previous_cpu: CpuTimes::read()?,
            latest: None,
            window: Duration::from_secs(60),
            interval: 1000,
        })
    }

    fn tick(&mut self) -> Result<()> {
        let current = CpuTimes::read()?;
        let usage = current.usage_since(self.previous_cpu);
        self.previous_cpu = current;
        let Some(cpu) = usage else { return Ok(()) };
        let (used, total) = memory()?;
        self.latest = Some((cpu, used, total));
        let at = self.started.elapsed();
        self.history.push(Sample {
            at,
            cpu,
            memory: (used as f64 / total as f64 * 100.0) as f32,
        });
        self.redraw()
    }

    fn redraw(&self) -> Result<()> {
        let icon = Icon::from_rgba(
            self.history.rgba(self.started.elapsed(), self.window),
            SIZE as u32,
            SIZE as u32,
        )?;
        // set_icon replaces the owned icon; the previous HICON is dropped, not retained.
        if self.tray.set_icon(Some(icon.clone())).is_err() {
            // Ask the wrapper to restore its icon after an unavailable/restarted Shell.
            // SAFETY: Synchronous message to our own live tray window, no pointers.
            unsafe {
                SendMessageW(
                    self.tray.window_handle(),
                    RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
                    0,
                    0,
                );
            }
            self.tray
                .set_icon(Some(icon))
                .map_err(|error| format!("update tray icon: {error}"))?;
        }
        if let Some((cpu, used, total)) = self.latest {
            self.tray.set_tooltip(Some(format!(
                "CPU {cpu:.1}% | 内存 {:.1}%\n{:.1} / {:.1} GiB\n窗口 {}秒 | 间隔 {}秒",
                used as f64 / total as f64 * 100.0,
                used as f64 / 1073741824.0,
                total as f64 / 1073741824.0,
                self.window.as_secs(),
                f64::from(self.interval) / 1000.0
            )))?;
        }
        Ok(())
    }

    fn command(&mut self, code: usize, timer: &mut Timer) -> Result<bool> {
        match code {
            1 => return Ok(false),
            2 => {
                // Muda toggles check marks before delivering the click. Derive the
                // requested state from the registry, and restore the mark on failure.
                let change = startup::enabled()
                    .and_then(|enabled| startup::set_enabled(!enabled).map(|()| !enabled));
                match change {
                    Ok(enabled) => self.startup_enabled = enabled,
                    Err(error) => {
                        self.startup_enabled = startup::enabled().unwrap_or(self.startup_enabled);
                        show_error(&format!("无法更改开机自启：{error}"));
                    }
                }
                self.startup_item.set_checked(self.startup_enabled);
                return Ok(true);
            }
            100..200 => {
                let index = code - 100;
                if let Some(seconds) = WINDOWS.get(index) {
                    self.window = Duration::from_secs(*seconds);
                    for (i, item) in self.window_items.iter().enumerate() {
                        item.set_checked(i == index);
                    }
                }
            }
            200..300 => {
                let index = code - 200;
                if let Some(ms) = INTERVALS.get(index) {
                    // Create the replacement first, so failure never leaves no timer.
                    *timer = Timer::new(*ms)?;
                    self.interval = *ms;
                    for (i, item) in self.interval_items.iter().enumerate() {
                        item.set_checked(i == index);
                    }
                }
            }
            _ => {}
        }
        self.redraw()?;
        Ok(true)
    }
}

pub fn run() -> Result<()> {
    // SAFETY: This is the sole UI thread; Shell extensions may require an STA.
    let com_result = unsafe { CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32) };
    if com_result < 0 {
        return Err(format!("COM initialization: {com_result:#x}").into());
    }
    let _apartment = ComApartment;
    let smoke = std::env::args().any(|argument| argument == "--smoke-test");
    if smoke {
        std::fs::write(
            std::env::current_exe()?.with_extension("smoke-test.txt"),
            "",
        )?;
    }
    // SAFETY: The name is NUL-terminated; no initial ownership or custom security.
    let raw = unsafe {
        CreateMutexW(
            ptr::null(),
            0,
            wide("Local\\CpuMemoryTray-8904d2a3").as_ptr(),
        )
    };
    if raw.is_null() {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: Read immediately after CreateMutexW, before any other Windows call.
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let _instance = OwnedHandle(raw);
    if existed {
        return Ok(());
    }

    let mut message = MSG::default();
    // SAFETY: Create this thread's message queue before posting menu commands.
    unsafe {
        PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_NOREMOVE);
    }
    // SAFETY: Returns the current thread identifier without acquiring a handle.
    let thread = unsafe { GetCurrentThreadId() };
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let id = event.id.0.as_str();
        let code = if id == "quit" {
            Some(1)
        } else if id == "startup" {
            Some(2)
        } else if let Some(value) = id
            .strip_prefix("window:")
            .and_then(|s| s.parse::<u64>().ok())
        {
            WINDOWS.iter().position(|s| *s == value).map(|i| 100 + i)
        } else if let Some(value) = id
            .strip_prefix("interval:")
            .and_then(|s| s.parse::<u32>().ok())
        {
            INTERVALS.iter().position(|s| *s == value).map(|i| 200 + i)
        } else {
            None
        };
        if let Some(code) = code {
            // SAFETY: Same process, valid queue, only integer payload; no borrowed pointers.
            unsafe {
                PostThreadMessageW(thread, COMMAND, code, 0);
            }
        }
    }));
    // Mouse move events would otherwise accumulate in the crate's unbounded channel.
    TrayIconEvent::set_event_handler(Some(|_: TrayIconEvent| {}));

    let initial = if smoke {
        // Shell/COM libraries keep process-wide caches after their first use.
        // Warm those once so the baseline measures application-owned resources.
        let warm = App::new()?;
        warm.redraw()?;
        drop(warm);
        Some(resources()?)
    } else {
        None
    };
    let mut app = App::new().map_err(|error| format!("create app: {error}"))?;
    let mut timer = Timer::new(app.interval).map_err(|error| format!("create timer: {error}"))?;
    if smoke {
        return smoke_test(app, timer, initial.unwrap());
    }
    loop {
        // SAFETY: Valid MSG output; this thread owns the GUI objects it dispatches to.
        let result = unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) };
        if result == -1 {
            return Err(io::Error::last_os_error().into());
        }
        if result == 0 {
            break;
        }
        if message.hwnd.is_null() && message.message == WM_TIMER && message.wParam == timer.0 {
            // Explorer can temporarily disappear/restart. Retry on the next timer
            // rather than terminating the monitor or spinning on an error.
            let _ = app.tick();
        } else if message.hwnd.is_null() && message.message == COMMAND {
            if !app.command(message.wParam, &mut timer)? {
                break;
            }
        } else {
            // SAFETY: Dispatch the initialized message received from GetMessageW.
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
    Ok(()) // timer, tray, history, menus and mutex all drop on this thread.
}

#[derive(Debug)]
struct Resources {
    handles: u32,
    gdi: u32,
    user: u32,
    private_bytes: usize,
}
fn resources() -> Result<Resources> {
    let mut handles = 0;
    let mut memory = PROCESS_MEMORY_COUNTERS::default();
    memory.cb = mem::size_of_val(&memory) as u32;
    // SAFETY: Pseudo-handle needs no close; outputs have correct sizes/lifetimes.
    unsafe {
        let process = GetCurrentProcess();
        if GetProcessHandleCount(process, &mut handles) == 0
            || GetProcessMemoryInfo(process, &mut memory, mem::size_of_val(&memory) as u32) == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Resources {
            handles,
            gdi: GetGuiResources(process, 0),
            user: GetGuiResources(process, 1),
            private_bytes: memory.PagefileUsage,
        })
    }
}

// Small, bounded native-resource regression check. Normal launches never run this.
fn smoke_test(mut app: App, mut timer: Timer, initial: Resources) -> Result<()> {
    let mut message = MSG::default();
    let pump = |message: &mut MSG| {
        // SAFETY: Drain pending GUI messages on their owning thread.
        unsafe {
            while PeekMessageW(message, ptr::null_mut(), 0, 0, 1) != 0 {
                TranslateMessage(message);
                DispatchMessageW(message);
            }
        }
    };
    std::thread::sleep(Duration::from_millis(500));
    app.tick()?;
    for _ in 0..25 {
        app.redraw()?;
        pump(&mut message);
    }
    let before = resources()?;
    for i in 0..500 {
        app.command(100 + i % WINDOWS.len(), &mut timer)?;
        app.command(200 + i % INTERVALS.len(), &mut timer)?;
        app.tick()?;
        pump(&mut message);
        std::thread::sleep(Duration::from_millis(2));
    }
    let after = resources()?;
    let stable = after.handles <= before.handles + 2
        && after.gdi <= before.gdi + 2
        && after.user <= before.user + 2
        && after.private_bytes <= before.private_bytes + 2 * 1024 * 1024;
    drop(timer);
    drop(app);
    pump(&mut message);
    let cleaned = resources()?;
    // Class registration and crate-wide initialization can leave a few fixed objects.
    // Per-update objects must already be gone; per-app objects must now be gone too.
    let released = cleaned.handles <= initial.handles + 3
        && cleaned.gdi <= initial.gdi + 1
        && cleaned.user <= initial.user + 3;
    std::fs::write(
        std::env::current_exe()?.with_extension("smoke-test.txt"),
        format!(
            "initial: {initial:?}\nbefore: {before:?}\nafter: {after:?}\ncleaned: {cleaned:?}\nstable: {stable}\nreleased: {released}\n"
        ),
    )?;
    if !stable || !released {
        return Err("Resource regression check failed; see smoke-test.txt".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::CpuTimes;
    #[test]
    fn cpu_uses_kernel_plus_user_with_idle_subtracted() {
        let before = CpuTimes {
            idle: 50,
            total: 100,
        };
        assert_eq!(
            CpuTimes {
                idle: 70,
                total: 200
            }
            .usage_since(before),
            Some(80.0)
        );
        assert_eq!(before.usage_since(before), None);
        assert_eq!(
            before.usage_since(CpuTimes {
                idle: 100,
                total: 300
            }),
            None
        );
    }
}
