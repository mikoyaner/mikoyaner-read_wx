//! C ABI 公共设施：状态码、出参内存约定、panic 防护。
//!
//! 协议依据 `docs/protocol/01-core-sessions-messages.md` §0：
//! - 所有 out 字符串 = DLL 分配的 NUL 结尾 UTF-8，唯一归还通道 `wcdb_free_string`；
//! - rc==0 时 outJson 不允许空指针（空结果集也要给 `"[]"`/`"{}"`）；
//! - rc!=0 时 TS 不读输出内容，错误文案可写入 outError（纯文本）。
//!
//! 这里的辅助函数都声明为 **safe fn**（内部用 unsafe 块），因为它们要在
//! `with_account`/`guard` 传入的闭包里调用——闭包体不会继承外层 `unsafe extern fn`
//! 的 unsafe 上下文。契约：out 指针由 TS 保证有效或为 NULL；入参 C 字符串由 TS 保证
//! NUL 结尾或为 NULL。

use std::ffi::{c_char, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};

// ── 状态码（wcdbCore.ts formatInitProtectionError 映射表） ──
pub const OK: i32 = 0;
pub const ERR_GENERIC: i32 = -1;
pub const ERR_SESSION_DB_NOT_FOUND: i32 = -3;
pub const ERR_DB_OPEN_FAILED: i32 = -4;
pub const ERR_SCHEMA_UNREADABLE: i32 = -5;
pub const ERR_NO_SESSION_TABLE: i32 = -6;
/// 会话表缺排序时间字段；在消息接口中 = message schema mismatch。
pub const ERR_SCHEMA_MISMATCH: i32 = -7;
pub const ERR_NO_MESSAGE_DB: i32 = -3;
pub const ERR_PANIC: i32 = -100;
// ① 实现方式：一组 i32 常量构成状态码表，0=成功、负值=各类失败，与 wcdbCore.ts 的
//    formatInitProtectionError 映射表一一对应；ERR_SCHEMA_MISMATCH 一码两义——会话接口
//    表示缺排序时间字段，消息接口表示 message schema 不匹配。
// ② 关键程序解析：ERR_NO_MESSAGE_DB 与 ERR_SESSION_DB_NOT_FOUND 同为 -3，是语义别名
//    而非笔误，TS 按接口上下文解读同一个码；ERR_PANIC=-100 专门标记"Rust panic 被 guard
//    拦下"，与业务错误区分。新增错误码必须同步 TS 映射表，否则用户提示退化为通用文案。
// ③ 功能：定义 FFI 返回值协议，让 TS 把数字码翻译成中文用户提示。

/// panic 不得越过 FFI 边界：所有导出函数体都包在 guard 内。
pub fn guard<F: FnOnce() -> i32>(f: F) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(rc) => rc,
        Err(_) => ERR_PANIC,
    }
    // ① 实现方式：catch_unwind 捕获闭包内 panic——Ok(rc) 原样返回业务码，Err(_)（panic
    //    payload）统一压成 ERR_PANIC(-100)；AssertUnwindSafe 解除 F 对 UnwindSafe 的约束。
    // ② 关键程序解析：panic 越过 extern "C" 边界是 UB（运行时直接 abort 整个宿主进程），
    //    所以每个导出函数体都必须包 guard；AssertUnwindSafe 是"断言闭包被 unwind 后状态
    //    仍一致"——这里闭包只借用调用方栈上的指针/字符串，不持有跨 panic 的全局可变状态，
    //    断言成立。panic payload 被丢弃不记日志，定位靠各业务函数自己的 push_log。
    // ③ 功能：FFI 边界的 panic 防火墙，把崩溃降级成 -100 错误码返回给 TS。
}

/// 读取可空 C 字符串；NULL 按空串处理（协议允许部分入参为 NULL，如
/// contacts_compact 的空列表、delete_message 的 dbPathHint 空串）。
pub fn cstr<'a>(p: *const c_char) -> &'a str {
    if p.is_null() {
        return "";
    }
    unsafe { CStr::from_ptr(p) }.to_str().unwrap_or("")
    // ① 实现方式：NULL 直接返回空串；否则 unsafe CStr::from_ptr 从指针扫描到 NUL，
    //    再 to_str 校验 UTF-8，非法字节序列 unwrap_or("") 退化为空串。
    // ② 关键程序解析：返回 &'a str 是**无界生命周期**——本质是把调用方内存借成任意长存活
    //    的引用，契约要求 TS 保证字符串调用期有效且 NUL 结尾；DLL 侧绝不能把它存进长寿命
    //    结构（cloud_report 因此只当场取 len 后丢弃）。非 UTF-8 输入静默变空串而非报错，
    //    传错编码时表现为"参数为空"类错误；from_ptr 对野指针无防护，有效性全靠 TS 契约。
    // ③ 功能：把可空 C 字符串借读成 &str，NULL/坏编码统一按空串处理（协议允许部分入参 NULL）。
}

/// 分配 out 字符串：返回给 TS 的指针，之后必经 wcdb_free_string 归还。
pub fn alloc_out(s: &str) -> *mut c_void {
    // 内嵌 NUL 会截断 C 字符串语义，替换为空格保证 JSON/文本完整性
    let sanitized = if s.contains('\0') { s.replace('\0', " ") } else { s.to_string() };
    CString::new(sanitized)
        .unwrap_or_else(|_| CString::new("").unwrap())
        .into_raw()
        .cast::<c_void>()
    // ① 实现方式：上面先把内嵌 NUL 替换成空格（C 字符串遇 NUL 即截断），这里 CString::new
    //    分配堆内存并补终止符，into_raw 放弃所有权交出裸指针，cast 成 c_void 匹配协议出参类型。
    // ② 关键程序解析：这是 DLL→TS 的唯一字符串分配通道，指针必须经 wcdb_free_string 归还
    //    否则泄漏；sanitize 之后 CString::new 理论上不会再失败，unwrap_or_else 兜底返回
    //    空串（两个 unwrap 都作用于无 NUL 的串，不会 panic）。消息正文里合法的 NUL 会被
    //    改写成空格——内容保真让位于 C 语义完整性；contains 预判避免常见路径的多余拷贝。
    // ③ 功能：把 Rust 字符串转成调用方持有的 NUL 结尾 UTF-8 堆指针。
}

/// 把字符串写入 `_Out_ void**` 出参；out 为 NULL 时静默跳过（TS 不会这么调）。
pub fn write_out(out: *mut *mut c_void, s: &str) {
    if !out.is_null() {
        unsafe { *out = alloc_out(s) };
    }
    // ① 实现方式：出参二级指针非 NULL 才解引用，把 alloc_out 的结果写进槽位；NULL 静默跳过。
    // ② 关键程序解析：unsafe 仅这一处解引用，契约由 TS 保证 out 是有效的 void** 槽；
    //    每次调用都分配新内存——TS 若对同一槽位写两次而不 free 旧指针就泄漏，所以协议
    //    规定一次调用只写一次。"rc==0 时 outJson 必非空（空结果集也要 []/{}）"由各业务
    //    函数保证，这里只管写入动作本身。
    // ③ 功能：把 JSON/文本结果落到 FFI 出参槽。
}

/// 释放 alloc_out 分配的指针；容忍 NULL。
pub fn free_out(ptr: *mut c_void) {
    if !ptr.is_null() {
        unsafe { drop(CString::from_raw(ptr.cast::<c_char>())) };
    }
    // ① 实现方式：非 NULL 才 from_raw 重建 CString 所有权并立即 drop 释放堆内存；NULL 容忍。
    // ② 关键程序解析：from_raw 的前提是指针**必须来自 alloc_out 的 into_raw**——拿它释放
    //    TS 栈缓冲（如 VerifyUser 的 1024B）或同一指针二次释放都是 UB/双击崩溃，这就是
    //    "唯一归还通道"约束的由来；cast::<c_char> 只改类型不改地址。drop 之后 TS 不得再读。
    // ③ 功能：回收 alloc_out 分配的字符串内存，防止跨语言内存泄漏。
}

/// 公共出参收敛：Ok(json) → 写 outJson 返回 OK；Err → 记日志返回指定错误码。
/// 覆盖 tables/query 组绝大多数导出（rc!=0 时 TS 只把状态码拼进中文提示）。
pub fn emit_json(out: *mut *mut c_void, result: anyhow::Result<String>, err_code: i32) -> i32 {
    match result {
        Ok(json) => {
            write_out(out, &json);
            OK
            // ① 实现方式：成功分支——write_out 把 JSON 字符串写进出参槽，返回 OK。
            // ② 关键程序解析：json 由上游保证非空（空结果集也序列化成 "[]"/"{}"），
            //    满足"rc==0 时 outJson 必非空"的协议不变量；write_out 对 NULL 出参静默跳过。
            // ③ 功能：tables/query 组绝大多数导出的成功路径统一收敛。
        }
        Err(e) => {
            crate::push_log(&format!("wcdb error: {e}"));
            err_code
            // ① 实现方式：失败分支——anyhow 错误的 Display 全文记入环形日志，返回调用方
            //    指定的 err_code；不写出参（rc!=0 时 TS 不读输出内容）。
            // ② 关键程序解析：错误详情只进进程内环形日志（wcdb_get_logs 可取、不落盘），
            //    TS 只拿数字码拼中文提示；{e} 会展开整条错误链，上游文案已刻意不含密钥
            //    明文（见 open_sqlcipher），但可能含库路径——日志仅存本机内存，符合红线。
            // ③ 功能：失败路径统一收敛：日志留痕 + 状态码上抛。
        }
    }
}
