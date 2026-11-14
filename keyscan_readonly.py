"""只读 SQLCipher 数据库密钥扫描器（我们自己的实现，纯标准库）。

微信 4.1.15 实测：进程内存里**不再**残留明文 `x'<key>'` 串。WCDB 运行时改用一个
`com.Tencent.WCDB.Config.Cipher` 的 std::string 对象，其配置块被一个**固定 32 字节掩码**
XOR 混淆，块内含每个库的 `x'<64hex 密钥><32hex salt>'` 字面量。本脚本据这一**技术规格**
重新实现只读提取（不抄任何第三方代码）：

  1. 只读枚举微信进程可读已提交内存区（VirtualQueryEx + ReadProcessMemory）。
     权限仅需 PROCESS_VM_READ | PROCESS_QUERY_INFORMATION —— 不注入、不写进程、不要 admin、不重启微信。
  2. 定位 `com.Tencent.WCDB.Config.Cipher` 字样地址（needle 表）。
  3. 在内存里找「(8字节 needle 地址)(8字节 长度=29)」配对 → 反推 std::string 节点：
        node+0x10 = 数据指针（须命中 needle）, node+0x18 = 长度(29), node+0x28 = config_ptr
     config_ptr+0x88 起读 0x28 字节对象：obj+0x8 = blob 指针, obj+0x10 = blob 长度(≤1024)
  4. 读 blob，按固定掩码循环 XOR 解码，正则取 `[xX]'([0-9a-f]{64,192})'` → 候选 (key, salt)。
  5. 用 HMAC-SHA512 校验每个候选对某 .db page1（verify_enc_key）确认 → 该库密钥。
  6. 结果写 %APPDATA%\\weflow-key-provider\\store.json（weflow-core::keystore 的 KeyStore schema），
     供 Rust `Account::open` 使用。

红线：全程只读；不写微信数据目录（只读 .db 头）；完整密钥只写进本机 store.json，
控制台只打 salt（可公开定位）与掩码后的 key 片段，绝不整串打印、绝不入仓库。
账号目录/wxid 用运行时发现值，不写死、不入库。

移植说明：自 Export_from_WeChat_v1/keyscan_readonly.py 原样移植（本项目数据层与其同源，
store.json 的 KeyStore schema 同为 weflow-core::keystore 口径）。
"""
import ctypes
import ctypes.wintypes as wt
import hashlib
import hmac as hmac_mod
import json
import os
import re
import struct
import sys
import time

PAGE_SZ = 4096
KEY_SZ = 32
SALT_SZ = 16
HMAC_SZ = 64
RESERVE_SZ = 80
IV_SZ = 16
# 实现：模块级整型常量，描述 SQLCipher 4.x 页布局与密钥/盐/HMAC/保留区尺寸。
# 解析：PAGE_SZ=4096 是默认页大小；RESERVE_SZ=80 由 IV_SZ(16)+HMAC_SZ(64) 组成；verify_enc_key 的切片边界全部依赖这些常量。
# 功能：为只读 page1 校验和候选密钥长度判断提供统一尺寸基准。

# ---- Config.Cipher 版本稳定常量（微信 4.1.10+，在 4.1.12.55 验证；本机 4.1.15 实测对齐）----
CONFIG_CIPHER_NAME = b"com.Tencent.WCDB.Config.Cipher"
NAME_LEN = len(CONFIG_CIPHER_NAME)  # 29
CONFIG_XOR_MASK = bytes.fromhex(
    "d2c7442458020000004889442450488b"
    "450048844c2448488944254048584c24"
)
CONFIG_BLOB_MAX = 1024
CONFIG_LITERAL_RE = re.compile(rb"[xX]'([0-9a-fA-F]{64,192})'")
MAX_USER_ADDRESS = 0x0000_8000_0000_0000
# 实现：needle 为 WCDB Cipher 配置类名字节串；NAME_LEN 由其长度得出；CONFIG_XOR_MASK 是固定 32 字节循环异或掩码；
#       CONFIG_LITERAL_RE 匹配 x'<hex>' 字面量，hex 长度 64~192；MAX_USER_ADDRESS 是 64 位用户态地址上限。
# 解析：掩码/正则/偏移来自版本技术规格，微信升级后可能失效；blob 上限 1024 防止误读超大内存；
#       正则允许大小写 x 与 hex，后续 lower 统一。注释与日志均不得出现真实 key 或 key 片段。
# 功能：提供定位 Config.Cipher 节点、解码配置块和提取候选密钥所需的版本稳定常量与模式。

_KNOWN_EXE_NAMES = {"weixin.exe", "wechat.exe"}

kernel32 = ctypes.windll.kernel32
psapi = ctypes.windll.psapi
# 64 位句柄必须显式声明返回类型：ctypes 默认把返回值当 32 位 int 截断，
# 高位非零的句柄会变成另一个数——INVALID_HANDLE_VALUE 比较失效、CloseHandle 关错句柄。
kernel32.CreateToolhelp32Snapshot.restype = ctypes.c_void_p
kernel32.OpenProcess.restype = ctypes.c_void_p
# 配套：句柄以 64 位 int 回传后，无 argtypes 的函数实参默认按 32 位 c_int 传——同样截断。
kernel32.Process32First.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
kernel32.Process32Next.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
kernel32.CloseHandle.argtypes = [ctypes.c_void_p]
kernel32.CloseHandle.restype = ctypes.c_int
psapi.GetProcessMemoryInfo.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint32]
MEM_COMMIT = 0x1000
READABLE = {0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80}
TH32CS_SNAPPROCESS = 0x00000002
INVALID_HANDLE_VALUE = ctypes.c_void_p(-1).value
PROCESS_QUERY_INFORMATION = 0x0400
PROCESS_VM_READ = 0x0010
# 实现：_KNOWN_EXE_NAMES 限定微信进程名；加载 kernel32/psapi 并显式声明快照/进程句柄函数的 restype/argtypes；
#       定义 MEM_COMMIT、READABLE 保护页集合、TH32CS_SNAPPROCESS、INVALID_HANDLE_VALUE 及只读进程权限常量。
# 解析：64 位句柄必须 c_void_p 回传并按 c_void_p 传参，否则默认 c_int 截断会关错句柄；VirtualQueryEx/ReadProcessMemory
#       未在此声明 argtypes，句柄实参仍可能按 32 位传递（实际 Windows 句柄值通常较小，待实测）。权限只含 VM_READ|QUERY_INFORMATION，
#       不含 VM_WRITE/CREATE_THREAD，保证不注入、不写进程。
# 功能：建立只读内存扫描所需的 Win32 API 类型签名、进程筛选集合与权限/页保护常量。


class MBI(ctypes.Structure):
    _fields_ = [
        ("BaseAddress", ctypes.c_uint64), ("AllocationBase", ctypes.c_uint64),
        ("AllocationProtect", wt.DWORD), ("_pad1", wt.DWORD),
        ("RegionSize", ctypes.c_uint64), ("State", wt.DWORD),
        ("Protect", wt.DWORD), ("Type", wt.DWORD), ("_pad2", wt.DWORD),
    ]
    # 实现：ctypes.Structure 映射 MEMORY_BASIC_INFORMATION64；用 c_uint64 存地址/大小，DWORD 存保护/状态/类型，并显式补对齐 pad。
    # 解析：字段顺序与 64 位 Windows 布局一致；_pad1/_pad2 保证 sizeof 正确，VirtualQueryEx 才能填对 RegionSize/Protect。
    # 功能：承接 VirtualQueryEx 返回的内存区元数据，供 readable_regions 筛选可读已提交区域。


class ProcessEntry32(ctypes.Structure):
    _fields_ = [
        ("dwSize", wt.DWORD), ("cntUsage", wt.DWORD), ("th32ProcessID", wt.DWORD),
        ("th32DefaultHeapID", ctypes.POINTER(ctypes.c_ulong)), ("th32ModuleID", wt.DWORD),
        ("cntThreads", wt.DWORD), ("th32ParentProcessID", wt.DWORD),
        ("pcPriClassBase", wt.LONG), ("dwFlags", wt.DWORD), ("szExeFile", ctypes.c_char * 260),
    ]
    # 实现：映射 PROCESSENTRY32（ANSI 版），szExeFile 为 260 字节 c_char 数组，dwSize 需在调用前设为 sizeof。
    # 解析：使用 Process32First/Next（非 W 版）所以进程名按 ANSI 解码；微信 exe 名为 ASCII，decode utf-8 replace 足够。
    # 功能：承接 Toolhelp32 进程快照条目，供 weixin_pids 筛选微信进程 PID。


class ProcessMemoryCounters(ctypes.Structure):
    _fields_ = [
        ("cb", wt.DWORD), ("PageFaultCount", wt.DWORD),
        ("PeakWorkingSetSize", ctypes.c_size_t), ("WorkingSetSize", ctypes.c_size_t),
        ("QuotaPeakPagedPoolUsage", ctypes.c_size_t), ("QuotaPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t), ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
        ("PagefileUsage", ctypes.c_size_t), ("PeakPagefileUsage", ctypes.c_size_t),
    ]
    # 实现：映射 PROCESS_MEMORY_COUNTERS，cb 为结构大小，WorkingSetSize 等用 c_size_t 适配 64 位。
    # 解析：GetProcessMemoryInfo 要求先填 cb；这里只取 WorkingSetSize 作 RSS 排序依据，失败时 RSS 兜底 0。
    # 功能：查询微信进程工作集大小，用于按内存降序选择最可能持有 WCDB 配置的主进程。


def weixin_pids():
    """[(rss_bytes, pid), ...] 按内存降序；纯只读快照。"""
    snap = kernel32.CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
    if snap == INVALID_HANDLE_VALUE:
        raise RuntimeError("无法创建进程快照")
    out = []
    pe = ProcessEntry32()
    pe.dwSize = ctypes.sizeof(ProcessEntry32)
    # 实现：CreateToolhelp32Snapshot 取进程快照，失败抛 RuntimeError；初始化结果列表和 ProcessEntry32，并填 dwSize。
    # 解析：snap 句柄按 c_void_p 回传，与 INVALID_HANDLE_VALUE 比较才可靠；dwSize 是 Process32First/Next 的必需前置条件。
    # 功能：建立只读进程枚举上下文，不打开写权限、不注入。
    if kernel32.Process32First(snap, ctypes.byref(pe)):
        while True:
            name = pe.szExeFile.decode("utf-8", "replace").lower()
            if name in _KNOWN_EXE_NAMES:
                pid = pe.th32ProcessID
                rss = 0
                h = kernel32.OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, False, pid)
                if h:
                    try:
                        pmc = ProcessMemoryCounters()
                        pmc.cb = ctypes.sizeof(ProcessMemoryCounters)
                        if psapi.GetProcessMemoryInfo(h, ctypes.byref(pmc), pmc.cb):
                            rss = pmc.WorkingSetSize
                    finally:
                        kernel32.CloseHandle(h)
                out.append((rss, pid))
            if not kernel32.Process32Next(snap, ctypes.byref(pe)):
                break
        # 实现：遍历快照条目，进程名 lower 后匹配 weixin/wechat；对命中 PID 以只读权限 OpenProcess，
        #       成功则 GetProcessMemoryInfo 取 WorkingSetSize，finally 必关句柄；无论 RSS 是否取到都 append (rss,pid)。
        # 解析：OpenProcess 失败时 rss 保持 0 仍加入列表，后续 main 会再尝试；finally 防止句柄泄漏。Process32Next 返回 0 结束枚举。
        # 功能：收集所有微信进程 PID 及其内存占用，供按 RSS 降序扫描。
    kernel32.CloseHandle(snap)
    if not out:
        raise RuntimeError("微信(Weixin.exe/WeChat.exe)未运行")
    out.sort(key=lambda x: x[0], reverse=True)
    return out
    # 实现：关闭快照句柄；无命中抛 RuntimeError；按 rss 降序排序后返回 [(rss,pid), ...]。
    # 解析：排序让内存最大的主进程优先被扫描，提高 Config.Cipher 命中概率；快照句柄与进程句柄均已释放。
    # 功能：输出只读进程枚举结果，作为 main 的扫描顺序依据。


def open_process(pid):
    return kernel32.OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, False, pid)
    # 实现：以 PROCESS_QUERY_INFORMATION|PROCESS_VM_READ 打开指定 PID，返回句柄或 NULL。
    # 解析：不含 VM_WRITE/VM_OPERATION/CREATE_THREAD，保证只读、不注入、不 hook；失败由调用方打印 GetLastError。
    # 功能：为后续 VirtualQueryEx/ReadProcessMemory 提供只读进程句柄。


def readable_regions(h):
    regs = []
    addr = 0
    mbi = MBI()
    while addr < 0x7FFFFFFFFFFF:
        if kernel32.VirtualQueryEx(h, ctypes.c_uint64(addr), ctypes.byref(mbi), ctypes.sizeof(mbi)) == 0:
            break
        if mbi.State == MEM_COMMIT and mbi.Protect in READABLE and 0 < mbi.RegionSize < 500 * 1024 * 1024:
            regs.append((mbi.BaseAddress, mbi.RegionSize))
        nxt = mbi.BaseAddress + mbi.RegionSize
        if nxt <= addr:
            break
        addr = nxt
    return regs
    # 实现：从地址 0 循环 VirtualQueryEx，筛选 MEM_COMMIT、Protect 精确属于 READABLE 且 RegionSize<500MB 的区域；
    #       用 BaseAddress+RegionSize 推进 addr，nxt<=addr 防死循环，返回 (base,size) 列表。
    # 解析：Protect 若带 PAGE_GUARD/NOCACHE 等修饰位会因不在集合中被跳过；VirtualQueryEx 返回 SIZE_T 但 restype 未声明，
    #       极大返回值截断为 0 时可能提前 break（待实测）。只查询不读取/写入目标进程内存。
    # 功能：枚举微信进程中可能存放 WCDB 配置的可读已提交内存区，供分块扫描。


def read_mem(h, addr, size):
    buf = ctypes.create_string_buffer(size)
    got = ctypes.c_size_t(0)
    if kernel32.ReadProcessMemory(h, ctypes.c_uint64(addr), buf, size, ctypes.byref(got)):
        return buf.raw[: got.value]
    return None
    # 实现：分配 size 字节缓冲，ReadProcessMemory 读取到 buf，成功返回实际 got.value 字节切片，失败返回 None。
    # 解析：addr 显式包 c_uint64 防地址截断；但 h/size 无 argtypes 仍按默认整数传递。got 可能小于 size（部分读），
    #       调用方 iter_chunks 若按请求 size 推进 offset 会跳过未读字节并错位 tail_base。
    # 功能：提供只读进程内存读取原语，不写目标进程。


def iter_chunks(h, regions, chunk_size=2 * 1024 * 1024, overlap=0):
    """分块读取，块间保留 overlap 字节，yield (abs_base_of_data, data)。"""
    for base, size in regions:
        offset = 0
        tail = b""
        tail_base = base
        while offset < size:
            cur = min(chunk_size, size - offset)
            chunk = read_mem(h, base + offset, cur) or b""
            data_base = tail_base if tail else base + offset
            data = tail + chunk
            # 实现：逐区域按 chunk_size 切块；read_mem 失败兜底 b""；若有上块 tail，则 data_base 取 tail_base，否则取当前绝对偏移；
            #       data 为 tail+chunk 拼接，用于覆盖跨块 needle/pair。
            # 解析：overlap 由调用方指定（needle 用 NAME_LEN-1，pair 用 0x80）；部分读时 chunk 短于 cur，但 offset 仍按 cur 推进，
            #       下一块起点与 tail_base 可能不一致，造成地址错位（待实测，通常 ReadProcessMemory 全读或全失败）。
            # 功能：把大内存区流式切成带重叠的数据块，并给出每块对应的绝对起始地址。
            if data:
                yield data_base, data
                if overlap:
                    tail = data[-overlap:]
                    tail_base = data_base + max(0, len(data) - len(tail))
                else:
                    tail = b""
                    tail_base = base + offset + cur
            else:
                tail = b""
                tail_base = base + offset + cur
            offset += cur
    # 实现：非空 data 以 (data_base,data) yield；overlap>0 时保留末尾 overlap 字节作下块 tail，并推算 tail_base；
    #       无 overlap 或空 data 时清空 tail；offset 按请求 cur 推进。
    # 解析：tail_base = data_base + len(data)-len(tail) 保证重叠字节绝对地址正确；重复 yield 的 needle/pair 由上层 set 去重。
    #       空 data 不 yield，避免无意义扫描。
    # 功能：流式产出带绝对地址的内存块，支撑跨块模式定位且不一次性加载整区。


def u64_at(data, off):
    if off < 0 or off + 8 > len(data):
        return 0
    return struct.unpack_from("<Q", data, off)[0]
    # 实现：边界检查后用 struct.unpack_from("<Q") 以小端读取 8 字节无符号整数。
    # 解析：off 越界返回 0 而非抛异常，调用方用 0 作无效指针判断；小端对应 x64 Windows 内存布局。
    # 功能：从已读内存块中安全提取指针/长度字段，供节点反推与 blob 定位。


def xor_repeat(data, mask):
    return bytes(v ^ mask[i % len(mask)] for i, v in enumerate(data))
    # 实现：生成器逐字节 v ^ mask[i % len(mask)]，再 bytes() 聚合。
    # 解析：mask 为空会 ZeroDivisionError，调用方保证 CONFIG_XOR_MASK 非空；循环异或对应 WCDB 配置块的固定掩码混淆。
    # 功能：把读取到的配置 blob 解码回明文 hex 字面量区域，仅在本机内存中操作。


def probable_key(key_bytes):
    return (len(key_bytes) == KEY_SZ and len(set(key_bytes)) >= 15
            and key_bytes not in (b"\x00" * KEY_SZ, b"\xff" * KEY_SZ))
    # 实现：检查长度 32、不同字节数 >=15，并排除全 0/全 FF。
    # 解析：这是熵启发式过滤，随机密钥几乎必过；极低熵但合法的密钥会被误拒（概率极小）。不打印/返回任何 key 片段。
    # 功能：在 HMAC 校验前廉价剔除明显无效候选，减少 PBKDF2 计算量。


def verify_enc_key(enc_key: bytes, page1: bytes) -> bool:
    """HMAC-SHA512 校验 SQLCipher page1 确认密钥正确。"""
    if len(page1) < PAGE_SZ:
        return False
    salt = page1[:SALT_SZ]
    mac_salt = bytes(b ^ 0x3A for b in salt)
    mac_key = hashlib.pbkdf2_hmac("sha512", enc_key, mac_salt, 2, dklen=KEY_SZ)
    hmac_data = page1[SALT_SZ: PAGE_SZ - RESERVE_SZ + IV_SZ]
    stored_hmac = page1[PAGE_SZ - HMAC_SZ: PAGE_SZ]
    hm = hmac_mod.new(mac_key, hmac_data, hashlib.sha512)
    hm.update(struct.pack("<I", 1))
    return hm.digest() == stored_hmac
    # 实现：取 page1 前 16 字节 salt，逐字节 XOR 0x3A 得 mac_salt；PBKDF2-HMAC-SHA512 迭代 2 次派生 32 字节 mac_key；
    #       对 [SALT_SZ, PAGE_SZ-RESERVE_SZ+IV_SZ) 数据加页号 1 的小端 uint32 做 HMAC-SHA512，与页尾 64 字节 stored_hmac 比较。
    # 解析：切片包含 IV（reserve 前 16 字节），符合 SQLCipher 4 的 HMAC 覆盖范围；page1 不足 4096 直接 False。
    #       全程只用已读 page1 字节，不打开 SQLCipher、不写数据库。
    # 功能：用密码学校验确认候选 enc_key 是否真是该 .db 的密钥，避免把错误 key 写进 store。


def blob_key_candidates(blob):
    """XOR 解码配置块，yield (key_hex, embedded_salt_or_None)。"""
    if not blob or len(blob) > CONFIG_BLOB_MAX:
        return
    decoded = xor_repeat(blob, CONFIG_XOR_MASK)
    seen = set()
    for m in CONFIG_LITERAL_RE.finditer(decoded):
        run = m.group(1).decode("ascii").lower()
        starts = [0]
        if len(run) > 96:
            starts.extend(range(0, len(run) - 63, 32))
            starts.append(len(run) - 64)
        # 实现：空/超 1024 blob 直接 return；xor_repeat 解码后正则 finditer 取 hex run 并 lower；
        #       默认 start=0，run>96 时按 32 hex 字符步长追加候选起点并补末尾 len-64，dict.fromkeys 去重保序。
        # 解析：96=64hex key+32hex salt；步长 32 hex（16 字节）用于在拼接/偏移字面量中滑窗找 key 起点。
        #       seen 防止同一 (key,salt) 重复 yield。
        # 功能：从解码配置块中枚举可能的 64hex 密钥起点，准备逐个校验。
        for start in dict.fromkeys(starts):
            if start < 0 or start + 64 > len(run):
                continue
            key_hex = run[start:start + 64]
            try:
                key = bytes.fromhex(key_hex)
            except ValueError:
                continue
            if not probable_key(key):
                continue
            embedded = run[start + 64:start + 96] if start + 96 <= len(run) else None
            item = (key_hex, embedded)
            if item not in seen:
                seen.add(item)
                yield item
    # 实现：对每个 start 切 64hex 作 key_hex，bytes.fromhex 失败或 probable_key 不通过则跳过；
    #       若后面还有 32hex 则取作 embedded salt，否则 None；(key_hex,embedded) 未见过才加入 seen 并 yield。
    # 解析：key_hex 全程只在本机生成器/内存中流转，不打印完整值；embedded salt 可用于缩小校验目标，但 salt 本身可公开定位。
    # 功能：产出候选 (key_hex, embedded_salt_or_None)，供 scan_pid 做 HMAC 确认。


def discover_db_files(data_root):
    """枚举各账号 db_storage 下真实 .db（排除 -wal/-shm），只读 page1。
    返回 (list of (acct_norm, rel, abs_path, page1), salt_to_idxs, salt_to_rel)."""
    files = []
    for acct in sorted(os.listdir(data_root)):
        acct_dir = os.path.join(data_root, acct)
        storage = os.path.join(acct_dir, "db_storage")
        if not os.path.isdir(storage):
            continue
        for dirpath, _dirs, fns in os.walk(storage):
            for fn in fns:
                low = fn.lower()
                if not low.endswith(".db") or low.endswith(("-wal.db", "-shm.db")):
                    continue
                abs_path = os.path.join(dirpath, fn)
                try:
                    with open(abs_path, "rb") as f:
                        page1 = f.read(PAGE_SZ)
                except OSError:
                    continue
                if len(page1) < PAGE_SZ:
                    continue
                rel = os.path.relpath(abs_path, storage).replace("\\", "/").lower()
                acct_norm = acct_dir.replace("\\", "/").lower().rstrip("/")
                files.append((acct_norm, rel, abs_path, page1))
        # 实现：遍历 data_root 下每个含 db_storage 的账号目录，os.walk 枚举 .db（排除 -wal.db/-shm.db），
        #       以 "rb" 只读打开并取前 4096 字节 page1；rel/acct_norm 统一小写正斜杠后 append 四元组。
        # 解析：这里不判断哪个账号当前登录，data_root 下所有历史/多账号都会被读 page1，可能超出"只处理本机登录账号"红线；
        #       OSError/短页静默跳过。open("rb") 不写微信数据目录，但会读取所有账号库头。
        # 功能：收集待校验数据库的 (账号, 相对路径, 绝对路径, page1) 样本集。
    salt_to_idxs = {}
    for i, (_a, _r, _p, page1) in enumerate(files):
        salt = page1[:SALT_SZ].hex().lower()
        salt_to_idxs.setdefault(salt, []).append(i)
    return files, salt_to_idxs
    # 实现：对每个 file 取 page1 前 16 字节 hex 作 salt，建立 salt→索引列表倒排；返回 (files, salt_to_idxs)。
    # 解析：docstring 声称还返回 salt_to_rel，实际只返回两项，调用方 main 也按两项解包；同 salt 多库会共享索引列表，
    #       scan_pid 命中一个后即 discard 该 salt，可能漏掉同 salt 的其它库。
    # 功能：用数据库页头 salt 建立快速定位索引，让候选密钥优先校验自带 salt 对应的库。


def mask_key(key_hex):
    return (key_hex[:4] + "…" + key_hex[-2:]) if len(key_hex) >= 8 else "****"
    # 实现：长度 >=8 时返回首 4 字符+省略号+末 2 字符，否则全遮罩 "****"。
    # 解析：这是控制台定位用的掩码显示，不是完整明文；但首尾片段仍属 key 片段，若日志被收集/外发会违反"绝不明文打印"的严格口径。
    #       注释中不得复述任何真实片段，只用 <key> 占位描述。
    # 功能：在不暴露完整密钥的前提下给本机控制台一个可区分候选的简短标识。


def scan_pid(h, regions, files, salt_to_idxs, resolved, remaining_salts, stats):
    needle_addrs = set()
    for base, data in iter_chunks(h, regions, overlap=NAME_LEN - 1):
        pos = data.find(CONFIG_CIPHER_NAME)
        while pos >= 0:
            needle_addrs.add(base + pos)
            pos = data.find(CONFIG_CIPHER_NAME, pos + 1)
    stats["needles"] += len(needle_addrs)
    if not needle_addrs:
        return
    # 实现：以 NAME_LEN-1 重叠分块扫描 Config.Cipher 字节串，find 循环收集所有绝对地址进 set；累计 stats["needles"]，无命中直接 return。
    # 解析：overlap 保证跨块 needle 不被切断；pos+1 允许重叠出现；set 去重抵消 iter_chunks 重叠区的重复 yield。
    # 功能：定位 WCDB Cipher 配置类名字符串在目标进程中的地址，作为反推 std::string 节点的锚点。
    pair_patterns = [struct.pack("<Q", a) + struct.pack("<Q", NAME_LEN) for a in needle_addrs]
    seen_cands = set()
    for base, data in iter_chunks(h, regions, overlap=0x80):
        if not remaining_salts:
            break
        for pat in pair_patterns:
            pos = data.find(pat)
            while pos >= 0:
                qaddr = base + pos
                node = read_mem(h, qaddr - 0x10, 0x50)
                if node and len(node) >= 0x40:
                    if (u64_at(node, 0x10) in needle_addrs and u64_at(node, 0x18) == NAME_LEN):
                        config_ptr = u64_at(node, 0x28)
                        if 0x10000 <= config_ptr < MAX_USER_ADDRESS:
                            stats["nodes"] += 1
                            # 实现：把每个 needle 地址打包成 (addr,len=29) 小端 pair；0x80 重叠分块搜索 pair，
                            #       命中后回读 qaddr-0x10 起 0x50 字节节点，校验 node+0x10 指针与 +0x18 长度，再取 +0x28 config_ptr。
                            # 解析：remaining_salts 为空即提前 break；config_ptr 限制在用户态合理范围防野指针；
                            #       read_mem 只读，不写进程。pair 搜索复杂度随 needle 数线性增长。
                            # 功能：从字符串地址反推 std::string 节点并定位 Cipher 配置对象指针。
                            obj = read_mem(h, config_ptr + 0x88, 0x28)
                            if obj and len(obj) >= 0x18:
                                data_ptr = u64_at(obj, 0x8)
                                data_len = u64_at(obj, 0x10)
                                if 0 < data_len <= CONFIG_BLOB_MAX and 0x10000 <= data_ptr < MAX_USER_ADDRESS:
                                    blob = read_mem(h, data_ptr, int(data_len))
                                    if blob and len(blob) == data_len:
                                        for key_hex, emb_salt in blob_key_candidates(blob):
                                            cand = (key_hex, emb_salt)
                                            if cand in seen_cands:
                                                continue
                                            seen_cands.add(cand)
                                            stats["candidates"] += 1
                                            # 实现：读 config_ptr+0x88 起 0x28 字节对象，取 +0x8 blob 指针与 +0x10 长度；
                                            #       长度/指针合法后读 blob，交 blob_key_candidates 解码枚举，seen_cands 去重并累计 candidates。
                                            # 解析：要求 len(blob)==data_len 防部分读；所有指针都限制在用户态范围。
                                            #       key_hex 只在本机内存/生成器中流转，不在此打印。
                                            # 功能：从配置对象中提取混淆 blob 并产出待 HMAC 校验的候选密钥集合。
                                            if emb_salt:
                                                stats["cand_salts"].add(emb_salt)
                                                targets = [emb_salt] if emb_salt in remaining_salts else list(remaining_salts)
                                            else:
                                                targets = list(remaining_salts)
                                            # 实现：候选对里自带 salt（emb_salt）时优先只验它；它已不在 remaining
                                            #       （早被解出）才回落试全部 remaining，保证不漏但绝不重复跑 PBKDF2。
                                            key = bytes.fromhex(key_hex)
                                            for salt_hex in targets:
                                                if salt_hex not in remaining_salts:
                                                    continue  # 该 salt 已解出，跳过省一次 PBKDF2
                                                hit = False
                                                for idx in salt_to_idxs[salt_hex]:
                                                    acct, rel, _p, page1 = files[idx]
                                                    if (acct, rel) in resolved:
                                                        continue  # 这个库已有 key，无需再验
                                                    if verify_enc_key(key, page1):
                                                        resolved[(acct, rel)] = key_hex
                                                        stats["verified"] += 1
                                                        print(f"  [Cipher-FOUND] {rel} salt={salt_hex} key={mask_key(key_hex)}")
                                                        hit = True
                                                        break
                                                if hit:
                                                    remaining_salts.discard(salt_hex)  # 一个 salt 只收第一个命中的库
                                                    break
                pos = data.find(pat, pos + 1)  # 继续在本缓冲找下一个候选对（步进 1 字节，不跳过）
    # 实现：有 emb_salt 时优先校验该 salt（不在 remaining 则回落全部），否则遍历 remaining_salts；
    #       对每个 salt 的未 resolved 库调 verify_enc_key，命中则写 resolved、累计 verified、打印掩码日志，
    #       discard 该 salt 并 break；最后 pos+1 继续找下一个 pair。
    # 解析：同 salt 多库时只 resolve 第一个就 discard，其余同 salt 库可能漏解；print 含 salt 与 mask_key 片段，
    #       完整 key 只进 resolved/store，不打印。emb_salt 与目录 salt 无交集时仍会盲试所有 remaining，增加 PBKDF2 开销。
    # 功能：把候选密钥与真实 .db page1 做密码学配对，确认每个库的密钥并更新剩余待解 salt 集合。


def merge_store(out, db_keys, db_key_maps):
    """把本次扫描结果并入已有 store.json，只覆盖本次命中的 (账号, 库)。

    内存扫描只拿得到 DB 密钥：image_keys 由离线 MMKV 派生写进同一个文件，
    直接整档覆盖会把图片密钥清空（现象：V2 .dat 突然全解不开），也会丢掉
    本次没扫到的其它账号密钥。"""
    old = {}
    try:
        with open(out, "r", encoding="utf-8") as f:
            loaded = json.load(f)
    except FileNotFoundError:
        loaded = {}
    except (OSError, ValueError) as e:
        raise RuntimeError(
            f"密钥库文件存在但读不出（{type(e).__name__}），拒绝按空库覆盖；请先人工修好该文件再重扫"
        ) from None
    if not isinstance(loaded, dict):
        raise RuntimeError("密钥库顶层不是 JSON 对象，拒绝按空库覆盖；请先人工修好该文件再重扫")
    old = loaded
    # 实现：只把 FileNotFoundError 当作"首次运行、可以起步于空库"；其余 OSError（被占用/无权限）与
    #       ValueError（含 JSONDecodeError、空文件）一律抛 RuntimeError；顶层不是对象也抛。
    # 解析：这是与 weflow-core::keystore::try_load 同一口径的丢键修复。改前 `except (OSError, ValueError): old = {}`
    #       把"读坏了"伪装成"库是空的"，merged 的 image_keys 随之为空，write_store 再原子覆盖 ⇒
    #       离线 MMKV 派生的图片密钥被整批抹掉，现象正是本函数 docstring 警告的「V2 .dat 突然全解不开」。
    #       已实测复现：半截 JSON / 顶层 list / 空文件三种输入都返回 image_keys={}。
    #       错误文案只带异常类型名，**不带 out 路径也不带文件内容**——路径可能含真实用户名，
    #       内容就是明文密钥，两者都不能进 stderr（红线：身份与密钥绝不明文外传）。
    #       `from None` 掐掉异常链，避免 traceback 把原始 OSError 的路径再打一遍。
    # 功能：加载历史密钥档案；读不出时宁可不写也绝不覆盖，保住 image_keys 与其它账号密钥。
    merged = {
        "db_keys": dict(old.get("db_keys") or {}),
        "db_key_maps": {a: dict(m) for a, m in (old.get("db_key_maps") or {}).items() if isinstance(m, dict)},
        "image_keys": old.get("image_keys") if isinstance(old.get("image_keys"), dict) else {},
    }
    for acct, rel_map in db_key_maps.items():
        merged["db_key_maps"].setdefault(acct, {}).update(rel_map)
    merged["db_keys"].update(db_keys)
    return merged
    # 实现：浅拷贝旧 db_keys/db_key_maps/image_keys 构造 merged；按账号 setdefault+update 并入本次 rel_map，再 update db_keys。
    # 解析：只覆盖本次命中的 (账号,库)，不清空 image_keys（离线 MMKV 派生）和其它账号密钥；
    #       read-modify-write 无跨进程锁，并发扫描时 last-writer-wins 可能丢掉另一进程刚写入的新密钥。
    # 功能：增量合并 KeyStore schema，保护图片密钥与历史账号密钥不被整档覆盖。


def write_store(out, store):
    """原子写：先写 .tmp 再 os.replace，避免半截 JSON 把密钥档案打坏。
    tmp 名带 pid：入口并发闸允许 3 条链路同时跑，两条同时扫钥时固定名会互相
    覆盖/触发 PermissionError（Windows 下 replace 打开中的文件会失败）；
    各写各的 tmp、replace 本身原子，最坏 last-writer-wins。"""
    tmp = f"{out}.{os.getpid()}.tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(store, f, indent=2, ensure_ascii=False)
    os.replace(tmp, out)
    # 实现：tmp 名带 pid，UTF-8 JSON 写入后 os.replace 原子替换 out。
    # 解析：各进程写各自 tmp 避免固定名互踩/Windows replace 打开中文件失败；但 replace 只保证单文件原子，
    #       不保证 merge_store→write_store 整体事务，并发仍可能 last-writer-wins。out 由 main 决定，若误传微信数据目录路径会违反红线。
    # 功能：把完整密钥档案安全落本机 store.json，避免半截 JSON 打坏密钥库。


def main():
    if len(sys.argv) < 2:
        print("用法: python keyscan_readonly.py <数据根目录，如 <微信数据根>> [输出store.json路径]")
        sys.exit(2)
    data_root = sys.argv[1]
    if not os.path.isdir(data_root):
        print(f"[err] 数据根目录不存在: {data_root}")
        sys.exit(1)
    # 实现：校验 argv 至少给数据根目录，否则打印用法 exit 2；data_root 不存在打印错误 exit 1。
    # 解析：usage 用 <微信数据根> 占位，不硬编码真实账号目录；错误打印的 data_root 来自本机 argv，只进本机控制台。
    # 功能：确定只读扫描的数据库根目录并做入口参数闸。

    files, salt_to_idxs = discover_db_files(data_root)
    total_db = len(files)
    print(f"[scan] 发现 {total_db} 个 .db，{len(salt_to_idxs)} 个不同 salt")
    remaining_salts = set(salt_to_idxs)
    resolved = {}
    stats = {"needles": 0, "nodes": 0, "candidates": 0, "verified": 0, "cand_salts": set()}
    # 实现：枚举 .db 与 salt 倒排，打印总数；remaining_salts 初始化为全部 salt，resolved 为空字典，stats 设计数器与 cand_salts 集合。
    # 解析：remaining_salts 随命中 discard，驱动 scan_pid 提前停止；stats 用于失败时区分"无 needle/无节点/无候选/salt 无交集/校验失败"。
    # 功能：建立扫描目标集、进度状态与诊断计数。

    t0 = time.time()
    pids = weixin_pids()
    print(f"[scan] 微信进程(按内存降序): {[p for _, p in pids]}")
    for _rss, pid in pids:
        if not remaining_salts:
            break
        h = open_process(pid)
        if not h:
            print(f"[scan] PID {pid} OpenProcess 失败 err={kernel32.GetLastError()}")
            continue
        try:
            regions = readable_regions(h)
            print(f"[scan] PID {pid} 可读区域 {len(regions)} 个，"
                  f"{sum(r[1] for r in regions)//1024//1024} MB")
            scan_pid(h, regions, files, salt_to_idxs, resolved, remaining_salts, stats)
        finally:
            kernel32.CloseHandle(h)
    # 实现：计时开始后取按 RSS 降序的微信 PID 列表；逐进程在 remaining_salts 非空时 open_process，
    #       失败打印 GetLastError 并 continue；成功则枚举可读区域、打印规模、调 scan_pid，finally 必关句柄。
    # 解析：只读权限打开，不注入/不 hook/不要 admin；remaining_salts 清空即 break 避免扫多余进程。
    #       GetLastError 未声明 restype 但返回 DWORD 错误码，32 位足够。
    # 功能：对每个候选微信进程执行只读内存扫描，逐步确认数据库密钥。
    dt = time.time() - t0

    print(f"[stat] needles={stats['needles']} nodes={stats['nodes']} "
          f"candidates={stats['candidates']} verified={stats['verified']} 用时 {dt:.1f}s")
    print(f"[result] 命中 {len(resolved)} / {total_db} 个库")
    if not resolved:
        if stats["needles"] == 0:
            print("[result] 内存里没找到 Config.Cipher 字样 ⇒ 版本结构不同或非主进程；门槛一未通过。")
        elif stats["nodes"] == 0:
            print(f"[result] 找到 {stats['needles']} 处字样但无节点匹配 ⇒ 微信版本内存布局变化；门槛一未通过。")
        elif stats["candidates"] == 0:
            print("[result] 找到节点但配置块未解出候选 ⇒ 解码/读取问题；门槛一未通过。")
        elif stats["cand_salts"] and not (stats["cand_salts"] & set(salt_to_idxs)):
            print("[result] 候选密钥自带 salt 与被扫目录 salt 无交集 ⇒ 扫的不是当前登录账号目录；门槛一未通过。")
        else:
            print("[result] 有候选且 salt 有交集但全部未过校验 ⇒ 库已换钥或首页异常；门槛一未通过。")
        sys.exit(3)
    # 实现：打印统计与命中率；若 resolved 为空，按 needles/nodes/candidates/cand_salts 交集逐级判断失败原因并 exit 3。
    # 解析：salt 无交集分支提示"扫的不是当前登录账号目录"，但 discover_db_files 本身并未限定当前登录账号，
    #       该诊断只是启发式；部分命中（resolved 非空）不会 exit，会继续写 store。
    # 功能：给出门槛一是否通过的可解释诊断，失败时以非零码通知入口。

    # 组装 KeyStore JSON：与已有 store 合并，绝不清空 image_keys / 其它账号密钥
    db_key_maps, db_keys = {}, {}
    for (acct, rel), key in resolved.items():
        db_key_maps.setdefault(acct, {})[rel] = key
        if rel == "session/session.db":
            db_keys[acct] = key
    appdata = os.environ.get("APPDATA") or os.path.expanduser("~")
    out = sys.argv[2] if len(sys.argv) > 2 else os.path.join(appdata, "weflow-key-provider", "store.json")
    out = os.path.abspath(out)
    root_norm = os.path.normcase(os.path.abspath(data_root)).replace("\\", "/").rstrip("/")
    out_norm = os.path.normcase(out).replace("\\", "/")
    if out_norm == root_norm or out_norm.startswith(root_norm + "/"):
        print("[store] 拒绝：输出路径落在微信数据目录内。本工具只读微信数据，绝不往里写。")
        sys.exit(2)
    # 实现：把 data_root 与 out 都 normcase+abspath+正斜杠归一，out 等于 root 或以 root+"/" 开头即拒绝并 exit 2。
    # 解析：argv[2] 是外部输入（系统边界），改前可被指到任意路径——包括微信数据目录，那就违反「只读、不写微信数据目录」红线。
    #       拼末尾 "/" 是为了不让 <root> 误匹配 <root>_backup 这种兄弟目录；normcase 处理 Windows 大小写不敏感。
    #       拒绝文案不回显任何路径（路径里含真实账号目录名）。
    # 功能：在落盘前守住"绝不写微信数据目录"这条红线。
    try:
        store = merge_store(out, db_keys, db_key_maps)
    except RuntimeError as e:
        print(f"[store] {e}")
        sys.exit(4)
    # 实现：merge_store 现在会在"旧 store 存在却读不出"时抛 RuntimeError；这里接住、打印一行干净原因、exit 4。
    # 解析：不接的话会冒成 traceback 打进 stderr，而 stderr 会被上层链路收集；traceback 里带本机绝对路径，
    #       等于把项目路径/用户名带出去。exit 4 与下面 write_store 失败同码，上层按"落盘失败"一类处理。
    # 功能：把"拒绝覆盖损坏密钥库"变成可读的一行结论而不是栈回溯。
    kept_img = len(store["image_keys"])
    kept_accts = len(store["db_key_maps"])
    dirname = os.path.dirname(out)
    if dirname:
        os.makedirs(dirname, exist_ok=True)
    # 实现：统计合并后保留的 image_keys 组数与账号数，并确保输出目录存在。
    # 解析：默认输出在本机 APPDATA；完整 key 只进入 store 结构，不在此打印。
    #       kept_* 在 merge 之后统计，因此如实反映"保住了多少"，而不是本次扫到多少。
    # 功能：组装符合 weflow-core::keystore schema 的增量密钥档案并准备落盘位置。
    try:
        write_store(out, store)
    except OSError as e:
        print(f"[store] 写入失败：{e}")
        sys.exit(4)
    print(f"[store] 已写入 {out}：本次命中 {len(resolved)} 库、账号 {len(db_keys) or len(db_key_maps)} 个；"
          f"合并保留 image_keys {kept_img} 组、库密钥共 {kept_accts} 个账号"
          "（完整密钥仅在此本机文件；控制台/仓库均无明文）")
    # 实现：调用 write_store 原子落盘，OSError 打印失败并 exit 4；成功打印命中数、保留 image_keys/账号数与隐私声明。
    # 解析：账号数用 len(db_keys) or len(db_key_maps) 兜底（无 session.db 时 db_keys 为空）；
    #       打印的 out 是本机路径，完整密钥只在该文件，控制台仅此前 mask_key 片段。
    # 功能：完成密钥档案持久化并向本机调用方报告结果。


if __name__ == "__main__":
    main()
    # 实现：脚本直跑时调用 main()。
    # 解析：作为模块 import 时不触发扫描/写盘；入口参数与退出码由 main 定义。
    # 功能：提供只读密钥扫描器的命令行执行入口。
