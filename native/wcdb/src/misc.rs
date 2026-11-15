//! 监控管道 / cloud 云控 / VerifyUser（docs/protocol/04-tables-misc.md 尾部）。
//!
//! 这三组都**不依赖 open_account/handle**，且都是可选符号（缺失只降级对应功能）。
//!
//! 现状：
//! - **监控管道**：真实的 Windows Named Pipe 服务端 + `ReadDirectoryChangesW` 目录监控
//!   需要 `windows` crate 与实机联调，暂返回非 0（TS 据此关闭监控功能，行为优雅降级）。
//!   后续可在 misc.rs 内补齐：NDJSON 单向推送 `{action,table,...}`（协议见 04 §监控管道线上协议）。
//! - **cloud 三件套**：无上报端点，`init` 起一个幂等后台线程、`report` 收下并记日志后返回 0
//!   （TS 据此认为整批已接收并 splice），`stop` 停线程并 join。
//! - **VerifyUser**：安全默认——始终写入 `{"success":false,...}`，绝不自动通过（避免绕过应用锁）。

use crate::ffi::*;
use serde_json::json;
use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

// ── 监控管道（暂禁用） ──

#[no_mangle]
pub extern "C" fn wcdb_start_monitor_pipe() -> i32 {
    guard(|| {
        crate::push_log("wcdb_start_monitor_pipe: not implemented, monitor disabled");
        // 返回非 0 → startMonitor() 返回 false，TS 整体关闭监控（优雅降级）
        ERR_GENERIC
        // ① 实现方式：函数体整体包在 guard（catch_unwind）里，先 push_log 记一条"未实现"，
        //    再返回 ERR_GENERIC(-1)；不创建任何线程/管道，零副作用。
        // ② 关键程序解析：真实实现应是 Windows Named Pipe 服务端 + ReadDirectoryChangesW
        //    目录监控（需 windows crate 与实机联调），这里用"永远失败"占位；TS 的 startMonitor()
        //    拿到非 0 会整体关闭监控而不是反复重试，属协议约定的优雅降级。符号必须导出，
        //    否则 koffi 绑定时直接找不到函数。
        // ③ 功能：以显式失败告知上层"监控管道不可用"，让 TS 关闭增量监控、退回轮询/手动刷新。
    })
}

#[no_mangle]
pub extern "C" fn wcdb_stop_monitor_pipe() {
    // 幂等；未启动时安全
    let _ = guard(|| OK);
    // ① 实现方式：void 返回，guard 的返回值被 let _ = 丢弃；闭包体只有 OK，无实际清理动作。
    // ② 关键程序解析：start 从未成功过，所以没有管道/线程可停；保留符号与幂等语义，
    //    是为了让 TS 的 stopMonitor() 在任何状态下调用都安全（不 panic、不越 FFI 边界）。
    // ③ 功能：监控管道的对称停止入口（当前为空操作）。
}

#[no_mangle]
pub extern "C" fn wcdb_get_monitor_pipe_name(out_name: *mut *mut c_void) -> i32 {
    guard(|| {
        // 未启动管道：返回非 0，TS 回退固定名（但监控已关，不会真正连接）
        let _ = out_name;
        ERR_GENERIC
        // ① 实现方式：out_name 出参完全不写（let _ = 丢弃指针），直接返回 ERR_GENERIC；
        //    外层 guard 保证 panic 不越 FFI 边界。
        // ② 关键程序解析：协议约定 rc!=0 时 TS 不读输出内容，所以不写出参是安全的；
        //    TS 拿到失败会回退固定管道名，但因 start 已失败、监控整体关闭，不会真的去连接。
        //    若未来实现管道，这里必须改为 alloc_out 写入并走 wcdb_free_string 归还。
        // ③ 功能：查询监控管道名的占位实现，失败即触发 TS 的降级路径。
    })
}

// ── cloud 云控（无端点，接受即丢弃 + 记日志） ──

struct CloudHandle {
    stop: Arc<AtomicBool>,
    joiner: Option<JoinHandle<()>>,
}
// ① 实现方式：两个字段——stop 是与后台线程共享的原子停止旗标（Arc 让控制方与线程各持一份），
//    joiner 是 Option 包裹的线程句柄，take() 后置 None 防止二次 join。
// ② 关键程序解析：AtomicBool 只能"通知"停，不能"打断"std::thread::sleep——线程要睡满
//    当前一轮才会看到旗标（见 wcdb_cloud_init 的循环）；stop_cloud_locked 的 join 因此
//    可能等待长达 interval 秒，这是历史 300 秒卡死回归的根源（详见该处注释）。
// ③ 功能：cloud 后台线程的停止控制与回收句柄。

fn cloud_slot() -> &'static Mutex<Option<CloudHandle>> {
    static SLOT: OnceLock<Mutex<Option<CloudHandle>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
    // ① 实现方式：函数内 static OnceLock 惰性初始化，首次调用建 Mutex<Option<CloudHandle>>，
    //    之后每次返回同一 &'static 引用；Option 为 None 表示"未初始化/已停止"。
    // ② 关键程序解析：OnceLock 保证多线程下只初始化一次且无数据竞争；用进程级全局槽而不是
    //    挂在 Account 上，是因为 cloud 三件套不依赖 open_account/handle（协议 04 约定）。
    //    Mutex poisoned 时各调用方按各自策略降级（init 报错、stop 静默跳过）。
    // ③ 功能：进程级单例存放 cloud 线程句柄，供 init/report/stop/shutdown 共享。
}

#[no_mangle]
pub extern "C" fn wcdb_cloud_init(interval_seconds: i32) -> i32 {
    guard(|| {
        let interval = if interval_seconds > 0 { interval_seconds as u64 } else { 300 };
        let mut slot = match cloud_slot().lock() {
            Ok(s) => s,
            Err(_) => return ERR_GENERIC,
        };
        // ① 实现方式：interval_seconds>0 才采用，否则默认 300 秒；随后锁全局槽，
        //    poisoned（持锁线程曾 panic）时直接返回 ERR_GENERIC。
        // ② 关键程序解析：默认 300 秒与 TS 侧云控上报周期一致；这个值同时是后台线程
        //    每轮 sleep 的时长，直接决定 stop/shutdown 时 join 的等待上限（见下文）。
        //    锁在函数剩余部分一直持有，保证"检查是否已初始化 + 写入句柄"整体原子。
        // ③ 功能：确定周期参数并取得全局槽的独占访问。
        if slot.is_some() {
            // 幂等：已初始化直接成功
            return OK;
        }
        // ① 实现方式：槽里已有 CloudHandle 就直接 return OK，不新建线程、不改周期。
        // ② 关键程序解析：幂等设计让 TS 重复调 init（如重连场景）不会产生第二条线程；
        //    注意已初始化时新传入的 interval_seconds 被忽略——想改周期必须先 stop 再 init。
        // ③ 功能：保证 cloud 后台线程全进程至多一条。
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let joiner = std::thread::spawn(move || {
            // 无云控端点：周期空转，仅维持线程语义（stop/shutdown 能干净停掉）
            while !stop_thread.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(interval.min(3600)));
            }
        });
        // ① 实现方式：stop 旗标 Arc 克隆一份 move 进线程；线程循环 = 检查旗标(Relaxed) →
        //    sleep(interval.min(3600)) 秒；无端点、无上报，纯空转维持线程语义。
        // ② 关键程序解析（历史回归点，因果务必看清）：wcdb_shutdown 会经 stop_cloud →
        //    stop_cloud_locked 对本线程 join；std::thread::sleep **不可中断**，置 stop 旗标后
        //    线程仍要睡满当前一轮（默认 300s、上限 3600s）才退出，join 便阻塞同样时长——
        //    曾导致 testConnection/close/forceReopen 全部卡 300 秒。正确修法是改用
        //    Condvar::wait_timeout 让 stop 立刻唤醒线程；**当前代码仍是 AtomicBool+sleep，
        //    全仓库未见 Condvar，疑似该修复在结构搬迁时丢失**（只注释不改码，待实测定案）。
        //    行内注释"能干净停掉"与实际行为不符；Relaxed 序足够，因为只传一个布尔信号。
        // ③ 功能：模拟云控周期线程的存在性，让 stop/shutdown 的线程生命周期语义成立。
        *slot = Some(CloudHandle { stop, joiner: Some(joiner) });
        OK
        // ① 实现方式：把旗标与线程句柄装进 CloudHandle 写入全局槽，返回 OK。
        // ② 关键程序解析：写入发生在持锁状态下，与上面的 is_some 检查构成原子"检查再写入"；
        //    此后 report 不读槽（只记日志），stop/shutdown 通过 take() 消费这份句柄。
        // ③ 功能：完成 cloud 初始化，登记可回收的后台线程。
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_cloud_report(stats_json: *const c_char) -> i32 {
    guard(|| {
        // 字符串仅调用期有效：立即复制。收下即视为已接收（返回 0），TS 会 splice 该批。
        let payload = cstr(stats_json);
        let len = payload.len();
        crate::push_log(&format!("wcdb_cloud_report accepted {len} bytes (no endpoint, dropped)"));
        let _ = json!({}); // 保留 serde_json 依赖引用一致性
        OK
        // ① 实现方式：cstr 把 C 字符串按借用读入（NULL→空串、非法 UTF-8→空串），只取字节数
        //    记进环形日志，payload 本体立即丢弃；json!({}) 是保持 serde_json 引用的空操作。
        // ② 关键程序解析：C 字符串仅在调用期有效，不能存指针只能当场消费；返回 0 让 TS
        //    认为整批已接收并 splice 掉本地队列——没有真实端点，数据就此丢弃（协议允许）。
        //    日志只记长度不记内容，避免上报数据（可能含账号信息）进日志。
        // ③ 功能：云控上报的"收下即丢"实现，保证 TS 队列不积压。
    })
}

#[no_mangle]
pub extern "C" fn wcdb_cloud_stop() {
    let _ = guard(|| {
        stop_cloud_locked();
        OK
        // ① 实现方式：guard 包裹下调用 stop_cloud_locked()（置旗标 + join），返回值丢弃。
        // ② 关键程序解析：void 导出不允许 panic 越界，guard 兜底；未初始化时
        //    stop_cloud_locked 内部 take() 得 None，安全空操作（幂等）。
        //    join 的阻塞上限问题见 stop_cloud_locked 处注释。
        // ③ 功能：TS 主动停止 cloud 线程的入口。
    });
}

fn stop_cloud_locked() {
    if let Ok(mut slot) = cloud_slot().lock() {
        if let Some(mut handle) = slot.take() {
            handle.stop.store(true, Ordering::Relaxed);
            if let Some(j) = handle.joiner.take() {
                let _ = j.join();
            }
        }
    }
    // ① 实现方式：锁全局槽 → take() 取出句柄（槽变 None，天然幂等）→ stop 旗标置 true →
    //    joiner.take() 后 join 等待线程退出；锁 poisoned 或槽为空时整段静默跳过。
    // ② 关键程序解析：join 是**同步阻塞**——线程正睡在 sleep(interval) 里（默认 300s），
    //    旗标无法打断 sleep，本函数就要等满剩余睡眠才返回；wcdb_shutdown 在 FFI 调用线程上
    //    直接调它，于是 shutdown 卡住 → TS 的 testConnection/close/forceReopen 全部超时，
    //    这正是历史回归的病灶。当前版本未见 Condvar 唤醒机制，阻塞路径疑似依然存在
    //    （修复可能在搬迁时丢失，待实测定案，此处只注释不改码）。join 结果 let _ = 吞掉：
    //    线程 panic 也不算 stop 失败；两次 take 保证不会重复 join（重复 join 会 panic）。
    // ③ 功能：回收 cloud 后台线程，确保资源释放后线程不再存活。
}

/// wcdb_shutdown 前调用，确保后台线程不会在资源释放后仍访问。
pub(crate) fn stop_cloud() {
    stop_cloud_locked();
    // ① 实现方式：crate 内部薄封装，直接转发 stop_cloud_locked。
    // ② 关键程序解析：供 lib.rs 的 wcdb_shutdown 在释放账号注册表**之前**调用，
    //    保证后台线程不会在资源释放后仍访问全局状态；名字里的 locked 指内部自行加锁，
    //    调用方无需持锁。
    // ③ 功能：shutdown 流程的"先停线程"步骤。
}

// ── VerifyUser（PascalCase 导出名，调用方分配 1024B 缓冲区，不用 free_string） ──

#[no_mangle]
pub unsafe extern "C" fn VerifyUser(
    hwnd: i64,
    message: *const c_char,
    out_result: *mut c_char,
    max_len: i32,
) {
    let _ = hwnd;
    let _ = cstr(message);
    // 安全默认：绝不自动通过（未实现 Windows Hello / UserConsentVerifier）。
    let body = json!({ "success": false, "error": "当前数据服务未实现身份验证" }).to_string();
    write_caller_buffer(out_result, &body, max_len);
    // ① 实现方式：hwnd 与 message 入参都只借读不验证（cstr 读后丢弃）；构造固定 JSON
    //    {"success":false,...} 序列化后经 write_caller_buffer 写进调用方缓冲区。
    // ② 关键程序解析：这是应用锁的身份验证钩子（PascalCase 导出名是 TS 侧约定）；
    //    安全默认 = 永远失败、绝不自动通过，否则等于绕过应用锁。真实实现应接
    //    Windows Hello / UserConsentVerifier 弹窗。本函数是 unsafe extern 且不走 guard，
    //    安全性完全依赖 write_caller_buffer 内的边界校验；出参由调用方分配（1024B），
    //    不经 alloc_out/wcdb_free_string 通道。
    // ③ 功能：以显式"未实现验证"拒绝所有验证请求，保持应用锁不被绕过。
}

/// 向调用方分配的缓冲区写入 NUL 结尾字符串，严格不越过 max_len（含 NUL）。
unsafe fn write_caller_buffer(out: *mut c_char, s: &str, max_len: i32) {
    if out.is_null() || max_len <= 0 {
        return;
    }
    // ① 实现方式：出参 NULL 或容量非正直接 return，一个字节都不写。
    // ② 关键程序解析：调用方可能传 NULL/0，这里必须静默容忍而不是解引用；
    //    max_len 是 i32，负数若直接 as usize 会变成天文数字导致越界写，所以先挡掉。
    // ③ 功能：unsafe 写入前的第一道边界防线。
    let cap = max_len as usize;
    let bytes = s.as_bytes();
    let n = bytes.len().min(cap.saturating_sub(1));
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.cast::<u8>(), n);
    *out.add(n) = 0;
    // ① 实现方式：容量转 usize；拷贝长度 n = min(内容字节数, cap-1)（saturating_sub 防 0-1
    //    下溢），copy_nonoverlapping 拷 n 字节后在第 n 位补 NUL，总写入 ≤ max_len（含终止符）。
    // ② 关键程序解析：本文件唯一的 unsafe 写边界——前提由调用方保证 out 指向 ≥max_len 的
    //    可写内存；截断按字节发生，可能把多字节 UTF-8 汉字切成半个（调用方 1024B 足够装下
    //    这条固定错误 JSON，正常不触发）。超长时静默截断而非报错，两指针不重叠故用
    //    copy_nonoverlapping（比 copy 稍快且语义明确）。
    // ③ 功能：把验证结果安全写回调用方分配的缓冲区，严格不越界。
}
