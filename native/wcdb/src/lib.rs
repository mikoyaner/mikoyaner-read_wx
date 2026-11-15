//! wcdb.dll —— WeFlow `wcdbLibPath` 组件（koffi C ABI）。
//!
//! 协议规格：本仓库 `docs/protocol/01~05-*.md`（逐函数对照 wcdbCore.ts 提取）。
//! 全局约定见 ffi.rs 头注释：状态码 0=成功、out 字符串唯一经 wcdb_free_string 归还、
//! rc==0 时 outJson 必非空、panic 不越 FFI 边界。
//!
//! 移植说明：自 Export_from_WeChat_v1/native/wcdb-native 原样移植，两处适配——
//! ① crate-type 改 rlib（见 Cargo.toml）；② 各业务模块改 pub mod，让本项目
//! wxmsg-exporter 能直接复用 contacts::resolve_group_members 等内省实现（其余内部逻辑未动）。

pub mod ffi;
pub mod contacts;
pub mod media;
pub mod messages;
pub mod misc;
pub mod query;
pub mod sessions;
pub mod stats;
pub mod tables;

use ffi::*;
use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use weflow_core::db::Account;

// ── 全局账号注册表 ──
// WeFlow 同一时刻只维护一个活动 handle，但 testConnection 会产生临时 handle，
// 且 shutdown 要能释放全部资源，因此用注册表而非单例。
// 所有调用来自同一 Node worker 线程并串行化，全局 Mutex 足够。

static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);

fn registry() -> &'static Mutex<HashMap<i64, Arc<Account>>> {
    static REG: OnceLock<Mutex<HashMap<i64, Arc<Account>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
    // ① 实现方式：函数内 static OnceLock 惰性建 Mutex<HashMap<i64, Arc<Account>>>，
    //    之后每次返回同一 &'static 引用；句柄→账号的注册表全进程唯一。
    // ② 关键程序解析：用注册表而非单例，是因为 testConnection 会开临时 handle 且 shutdown
    //    要释放全部资源；值用 Arc 让 get_account 克隆引用后立刻放锁，查询期间不持全局锁。
    //    所有 FFI 调用来自同一 Node worker 线程串行化，Mutex 只是防御性兜底；poisoned
    //    时各调用方按各自策略降级（open/close 报错、get_account 返回 None）。
    // ③ 功能：进程级账号句柄注册表，支撑多 handle 并存与统一回收。
}

fn get_account(handle: i64) -> Option<Arc<Account>> {
    registry().lock().ok()?.get(&handle).cloned()
    // ① 实现方式：锁注册表 → 按句柄查表 → 命中则 Arc::clone（只加引用计数）返回；
    //    锁 poisoned 或句柄不存在都返回 None。
    // ② 关键程序解析：返回克隆的 Arc 而非 MutexGuard，锁在函数返回时即释放——后续查询
    //    再慢也不会阻塞其它句柄的注册/注销；句柄由 NEXT_HANDLE 单调递增发放、shutdown
    //    后不重置，旧句柄不会误命中新账号（查表落空 → ERR_GENERIC）。
    // ③ 功能：句柄到账号上下文的安全路由。
}

/// 查询/写入类导出的公共入口：handle 路由 + panic 防护。
/// 后续 messages/sessions/contacts 等模块通过闭包实现各自逻辑。
pub(crate) fn with_account<F: FnOnce(&Account) -> i32>(handle: i64, f: F) -> i32 {
    guard(|| match get_account(handle) {
        Some(account) => f(&account),
        None => ERR_GENERIC,
    })
    // ① 实现方式：guard（panic 防火墙）内先 get_account 路由——命中把 &Account 交给业务
    //    闭包 f 并返回其业务码，落空返回 ERR_GENERIC。
    // ② 关键程序解析：messages/sessions/contacts 等所有带 handle 的导出共用这一入口，
    //    保证"无效句柄"与"业务 panic"两类失败都不会越 FFI 边界；闭包拿到的是 Arc 克隆
    //    支撑的借用，执行期间账号不会被 shutdown 释放（Arc 引用计数保活）。
    // ③ 功能：查询/写入类导出的公共骨架：句柄校验 + panic 防护。
}

// ── 生命周期（docs/protocol/01 §1-§7） ──

#[no_mangle]
pub extern "C" fn wcdb_init() -> i32 {
    guard(|| {
        // 注册表惰性初始化，无全局引擎状态需要预热。
        // 必须支持与 shutdown 多次成对重入。
        let _ = registry();
        OK
        // ① 实现方式：只触发 registry() 的 OnceLock 惰性初始化（let _ = 丢弃引用），
        //    不打开任何库、不起任何线程，返回 OK。
        // ② 关键程序解析：SQLCipher/SQLite 无全局引擎预热需求，init 的存在只为满足 TS
        //    生命周期协议（init→open→…→shutdown 可反复重入）；重复 init 幂等无害。
        // ③ 功能：DLL 生命周期起点，确保后续 open 可用的全局设施就绪。
    })
}

#[no_mangle]
pub extern "C" fn wcdb_shutdown() -> i32 {
    guard(|| {
        // 先停后台线程（cloud）与游标，再释放所有 handle、连接。
        misc::stop_cloud();
        stats::clear_cursors();
        if let Ok(mut reg) = registry().lock() {
            reg.clear();
        }
        OK
        // ① 实现方式：固定三步顺序——stop_cloud 停 cloud 线程 → clear_cursors 丢弃全部
        //    流式游标 → 锁注册表 clear() 丢掉所有 Arc<Account>（末个引用触发 Drop，连带
        //    关闭缓存的 SQLCipher 连接）；锁 poisoned 时跳过 clear 仍返回 OK。
        // ② 关键程序解析：顺序不可倒——先清账号再停线程会让线程访问已释放状态；先丢游标
        //    再关连接避免游标持有垂死句柄。**stop_cloud 内部 join 可能阻塞至多 interval 秒**
        //    （cloud 线程 sleep 不可中断，详见 misc.rs 回归点注释），shutdown 卡住会连坐
        //    TS 的 testConnection/close/forceReopen。NEXT_HANDLE 不重置，重开后句柄继续
        //    递增，旧句柄天然失效。
        // ③ 功能：DLL 生命周期终点，回收全部账号资源与后台线程。
    })
}

#[no_mangle]
pub extern "C" fn wcdb_purge_memory() -> i32 {
    guard(|| {
        if let Ok(reg) = registry().lock() {
            for account in reg.values() {
                account.shrink_memory();
            }
        }
        OK
        // ① 实现方式：锁注册表遍历所有账号，逐个调 shrink_memory（对每条已缓存连接执行
        //    PRAGMA shrink_memory 归还页缓存）；锁 poisoned 时整段跳过仍返回 OK。
        // ② 关键程序解析：只压内存不关连接、不动游标——正在流式读取的连接会被内层 Mutex
        //    阻塞等待其读完（见 db.rs shrink_memory）；持注册表锁期间逐库执行 PRAGMA，
        //    库多时其它 FFI 调用要排队。长期持有多条 SQLCipher 连接的常驻内存由此压低。
        // ③ 功能：响应 TS 的内存回收请求（wcdb_purge_memory），降低常驻内存水位。
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_open_account(
    path: *const c_char,
    key: *const c_char,
    handle: *mut i64,
) -> i32 {
    guard(|| {
        if handle.is_null() {
            return ERR_GENERIC;
        }
        *handle = 0;
        let path = cstr(path);
        let key = cstr(key);
        if path.is_empty() || key.is_empty() {
            return ERR_GENERIC;
        }
        // ① 实现方式：出参句柄槽 NULL 直接失败；先 unsafe 写 *handle=0（保证失败时 TS
        //    读到 0 而非未初始化值），再 cstr 借读路径与密钥，任一为空即失败。
        // ② 关键程序解析：key 只经 cstr→Account::open 流动，全程不进日志/不回显（红线：
        //    密钥绝不明文打印）；*handle=0 的 unsafe 解引用由前面的 NULL 检查护住。
        //    Account::open 内部会先 is_file 拦"路径不存在"，防止带 CREATE 标志的 open
        //    在微信数据目录凭空建空库（见 db.rs）。
        // ③ 功能：开账号入口的参数防线与出参初始化。
        match Account::open(path, key) {
            Ok(account) => {
                let id = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
                match registry().lock() {
                    Ok(mut reg) => {
                        reg.insert(id, account);
                        *handle = id;
                        OK
                        // ① 实现方式：原子递增发放新句柄 id → 锁注册表插入 Arc<Account> →
                        //    把 id 写回出参槽，返回 OK。
                        // ② 关键程序解析：Account::open 已完成 session 库验钥（每库一钥：
                        //    open 内部经 keystore 解析该库专属 key，主 key 只是兜底），
                        //    失败不会走到这里；fetch_add(Relaxed) 单调递增且 shutdown 不
                        //    重置，句柄全进程唯一不复用。锁 poisoned 时账号 Arc 被 Drop
                        //    （连接随之关闭）、句柄槽保持 0、返回 ERR_GENERIC，无泄漏。
                        // ③ 功能：登记已验钥的账号并把句柄交付 TS。
                    }
                    Err(_) => ERR_GENERIC,
                }
            }
            Err(e) => {
                // 错误分类映射到 TS formatInitProtectionError 的状态码
                let msg = e.to_string();
                let code = if msg.contains("不存在") {
                    ERR_SESSION_DB_NOT_FOUND
                } else if msg.contains("密钥错误") || msg.contains("open ") {
                    ERR_DB_OPEN_FAILED
                } else {
                    ERR_DB_OPEN_FAILED
                };
                // 详细原因进内部日志，wcdb_get_logs 可取（TS 在 open 失败时会读日志）
                push_log(&format!("wcdb_open_account failed: {msg}"));
                code
                // ① 实现方式：错误 Display 全文做子串分类——含"不存在"→-3（入口库缺失），
                //    其余一律 -4（打开失败）；分类后把全文 push_log 留痕，返回状态码。
                // ② 关键程序解析：中间分支（"密钥错误"/"open "）与 else 分支返回值**完全
                //    相同**，是冗余分类——实际语义只有"不存在→-3、其它→-4"两档，写成三档
                //    疑似预留细化但从未区分（不改码，仅指出）。子串匹配依赖 db.rs 的中文
                //    错误文案措辞，上游改文案这里就静默错分类；日志含库路径但不含密钥
                //    （上游错误串已按此口径构造），且只存进程内环形缓冲。
                // ③ 功能：把开账号失败翻译成 TS 可提示的状态码并留下排障日志。
            }
        }
    })
}

#[no_mangle]
pub extern "C" fn wcdb_close_account(handle: i64) -> i32 {
    guard(|| {
        // WeFlow 从不调用（历史崩溃已弃用），但符号必须存在且行为健壮。
        match registry().lock() {
            Ok(mut reg) => {
                reg.remove(&handle);
                OK
                // ① 实现方式：锁注册表按句柄 remove（不存在也返回 OK，幂等），丢弃的
                //    Arc 若是末个引用则 Drop 账号、关闭其全部缓存连接。
                // ② 关键程序解析：remove 不校验句柄有效性，传 0/野句柄都安全；若此刻
                //    另有查询持有该账号的 Arc 克隆，连接要等查询结束才真正关闭（引用
                //    计数语义）。WeFlow 主流程只用 shutdown 统一回收，本符号仅为 ABI
                //    完整性保留。
                // ③ 功能：按句柄注销单个账号（当前主流程未使用）。
            }
            Err(_) => ERR_GENERIC,
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_free_string(ptr: *mut c_void) {
    // 唯一的内存归还通道；void 返回，不允许 panic。
    let _ = guard(|| {
        free_out(ptr);
        OK
        // ① 实现方式：guard 包裹 free_out（from_raw 重建所有权并 drop），返回值丢弃。
        // ② 关键程序解析：ptr 必须是 alloc_out 交出的堆指针——TS 拿它释放栈缓冲或二次
        //    释放都是 UB，"唯一归还通道"的契约由 TS 侧遵守；NULL 容忍（free_out 内判）。
        //    void 导出没有错误码可报，guard 把潜在 panic 压成 ERR_PANIC 后丢弃，至少
        //    不会 abort 宿主进程。
        // ③ 功能：跨语言字符串内存的对称释放入口。
    });
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_set_my_account_id(handle: i64, account_id: *const c_char) -> i32 {
    with_account(handle, |account| {
        let id = cstr(account_id).trim().to_string();
        if let Ok(mut slot) = account.my_account_id.lock() {
            *slot = if id.is_empty() { None } else { Some(id) };
        }
        OK
        // ① 实现方式：with_account 路由后 cstr 借读账号 ID 并 trim 成自有 String，锁
        //    account.my_account_id 写入——空串清成 None、非空 Some(id)；poisoned 静默
        //    跳过仍返回 OK。
        // ② 关键程序解析：这是"我是谁"的后置注入点（open 时无法从密钥推导 wxid），
        //    messages 的 normalize_row 靠它判 computed_is_send（自己发的 vs 对方发的）；
        //    None 时只能回退 is_send 列或 Null。to_string 必须当场拷贝——cstr 的借用
        //    不能存活过本次 FFI 调用。
        // ③ 功能：告诉 DLL 当前账号的 <wxid>，供消息方向判定使用。
    })
}

// ── 内部日志环形缓冲（wcdb_get_logs 数据源，open 失败时 TS 会读取） ──

const LOG_CAPACITY: usize = 256;

fn log_buffer() -> &'static Mutex<Vec<String>> {
    static LOGS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    LOGS.get_or_init(|| Mutex::new(Vec::new()))
}

pub(crate) fn push_log(msg: &str) {
    if let Ok(mut logs) = log_buffer().lock() {
        if logs.len() >= LOG_CAPACITY {
            logs.remove(0);
        }
        logs.push(format!(
            "[{}] {msg}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        ));
    }
}

pub(crate) fn take_logs_json() -> String {
    match log_buffer().lock() {
        Ok(logs) => serde_json::to_string(&*logs).unwrap_or_else(|_| "[]".to_string()),
        Err(_) => "[]".to_string(),
    }
}
