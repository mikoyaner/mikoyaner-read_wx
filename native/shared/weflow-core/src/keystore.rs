//! 每库独立密钥库（微信 4.x：db_storage 下每个 .db 各有一把 SQLCipher key，
//! 实测确认 session.db 内无 keyring，session key 打不开其它库）。
//!
//! 这是 key-provider / wcdb.dll / welive-export **共享**的私有密钥通道：
//! WeFlow 只把 session.db 的单把 key 交给 `wcdb_open_account`，其余各库的 key 由
//! 用户经 key-provider 批量导入本库，本模块在打开每个库时按相对路径解析出对应 key，
//! 找不到就回退到传入的主 key（兼容单密钥 / 微信 3.x）。
//!
//! 存储位置（优先级，与 key-provider 一致）：
//!   1. 环境变量 `WEFLOW_KEY_STORE`
//!   2. 可执行文件同目录 `key-store.json`（若存在）
//!   3. `%APPDATA%\weflow-key-provider\store.json`（Unix: `~/.config/...`）

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 账号无关的默认桶（单账号或未区分时）。
pub const DEFAULT_ACCOUNT: &str = "__default__";
// ① 实现方式：固定字符串常量，作为 db_keys / image_keys 里"不区分账号"时的兜底桶名。
// ② 关键程序解析：用双下划线包裹避免与任何真实 <wxid> 撞名；candidate_db_keys 与 image_key
//    都会在按账号查不中时回退到这个桶。它**只用于按 accountId 索引的两张表**，
//    db_key_maps 是按账号目录索引的，没有默认桶概念。
// ③ 功能：给单账号/未指定账号的场景一个统一的兜底键。

/// 微信 .dat 图片密钥。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageKeyEntry {
    pub xor_key: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aes_key: Option<String>,
    // ① 实现方式：xor_key 用 u64 存（实际只用低 8 位），aes_key 是可选的 ASCII 文本密钥。
    // ② 关键程序解析：serde(default) 让旧 store 缺 aes_key 字段时反序列化为 None（V1 图片只需 xor）；
    //    skip_serializing_if 让 None 时不写该字段，保持 JSON 精简。
    //    **注意 derive(Debug)**：`{:?}` 会把 aes_key 明文打出来——上层禁止直接 Debug 打印本结构（红线：密钥绝不明文打印）。
    //    xor_key 是 u64 但 decrypt_dat 入参是 u32 且 &0xff，调用方负责截断。
    // ③ 功能：承载一个账号的图片解密材料（V1 的 XOR 键 + V2 的 AES 键）。
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct KeyStore {
    /// accountId(or DEFAULT) -> 主(session) key。WeFlow 打开账号用的那把；也作回退。
    #[serde(default)]
    pub db_keys: BTreeMap<String, String>,
    // ① 实现方式：BTreeMap（按键有序，序列化结果稳定）存"账号 → 主 session 密钥"。
    // ② 关键程序解析：serde(default) 保证缺字段时为空表而非反序列化失败；键是 accountId（<wxid> 或 __default__），
    //    **不做大小写/路径规范化**，与 db_key_maps 的键口径不同。值是明文 hex 密钥，绝不入库、绝不明文打印。
    // ③ 功能：保存每个账号的主密钥，供单 key 打开账号与每库密钥回退使用。
    /// normalizedAccountDir -> ( relPathUnderDbStorage -> hexKey )。每库独立密钥。
    #[serde(default)]
    pub db_key_maps: BTreeMap<String, BTreeMap<String, String>>,
    // ① 实现方式：两层 BTreeMap——外层键是 norm_path 规范化后的账号目录，内层键是 db_storage 下的相对路径。
    // ② 关键程序解析：微信 4.x **每库一钥**，session key 打不开其它库，所以必须有这张映射表；
    //    内外层键都经过规范化（小写 + 正斜杠），resolve_key 查询时也用同样规范化才能命中。
    //    内层相对路径形如 `message/message_0.db`，由 rel_under_db_storage 推导。
    // ③ 功能：承载"账号目录 → 各库密钥"的完整映射，是每库独立开库的依据。
    /// accountId(or DEFAULT) -> 图片密钥。
    #[serde(default)]
    pub image_keys: BTreeMap<String, ImageKeyEntry>,
    // ① 实现方式：BTreeMap 存"账号 → 图片密钥条目"，与 db_keys 同样按 accountId 索引。
    // ② 关键程序解析：键的口径在不同调用方间**并不统一**——reply-exporter 用账号目录 basename，
    //    key-provider 用 --account 传入的 <wxid>，两者若不一致会查不中而回退 __default__（见 image_key）。
    //    本结构 derive(Debug/Serialize)，序列化是为了写 store.json（唯一允许落盘的地方），Debug 打印则被红线禁止。
    // ③ 功能：保存各账号的图片解密材料。
}

/// 路径规范化：转正斜杠、转小写（Windows 大小写不敏感）、去尾部斜杠。
pub fn norm_path(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
    s.trim_end_matches('/').to_string()
    // ① 实现方式：路径转字符串 → 反斜杠转正斜杠 → 整体转小写 → 去掉尾部所有斜杠。
    // ② 关键程序解析：转小写是因为 Windows 文件系统大小写不敏感，同一目录可能有不同大小写写法；
    //    去尾斜杠保证 `<账号目录>` 与 `<账号目录>/` 归一成同一个键。
    //    **不处理** `\\?\` 前缀、8.3 短名、相对路径转绝对——这些变体会归一成不同的键而查不中映射表。
    // ③ 功能：产出 db_key_maps 外层键与 accounts_under 前缀比较用的规范路径。
}

fn norm_rel(s: &str) -> String {
    s.replace('\\', "/").to_ascii_lowercase().trim_start_matches('/').to_string()
    // ① 实现方式：与 norm_path 类似，但去的是**开头**斜杠（相对路径不应以 / 起始）。
    // ② 关键程序解析：用于把扫描器输出/调用方传入的相对库路径（如 `message\message_0.db`）
    //    归一成内层键 `message/message_0.db`；小写 + 正斜杠与 rel_under_db_storage 的产出口径一致。
    // ③ 功能：统一每库密钥映射的内层键写法。
}

/// 求 `db` 相对 `<account_dir>/db_storage` 的路径（小写正斜杠），如 `message/message_0.db`。
pub fn rel_under_db_storage(db: &Path, account_dir: &Path) -> Option<String> {
    let storage = account_dir.join("db_storage");
    let rel = db.strip_prefix(&storage).ok()?;
    // ① 实现方式：拼出 <账号目录>/db_storage，用 Path::strip_prefix 剥掉这段前缀得到相对路径。
    // ② 关键程序解析：strip_prefix 是**按组件、大小写敏感**的比较——若 db 路径里的 `DB_Storage`
    //    与这里拼的小写 `db_storage` 大小写不同，Windows 上文件照样能打开，但这里会失配返回 None，
    //    导致每库密钥查不中、回退主 key 然后开库失败（derive_account_dir 容忍大小写，这里却不兼容）。
    //    db 不在该 storage 下时也返回 None。
    // ③ 功能：推导每库密钥映射的内层键前半段。
    Some(norm_rel(&rel.to_string_lossy()))
    // ① 实现方式：把相对路径转字符串后过 norm_rel（小写 + 正斜杠 + 去头斜杠）。
    // ② 关键程序解析：与 put_db_key_map 写入时的归一完全一致，读写两侧口径必须相同才能命中。
    // ③ 功能：产出可直接查 db_key_maps 内层表的相对路径键。
}

/// 解析密钥库文件路径。
pub fn store_path() -> PathBuf {
    if let Ok(p) = std::env::var("WEFLOW_KEY_STORE") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    // ① 实现方式：优先级 1——环境变量 WEFLOW_KEY_STORE 非空则直接用它（允许显式指定 store 位置）。
    // ② 关键程序解析：trim 后判空，避免设了个空串就把 store 指到当前目录；
    //    这条路径**不受 APPDATA 约束**，调用方设到哪就读写到哪（save 也会写到这里）。
    // ③ 功能：给部署/测试一个覆盖默认位置的入口。
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("key-store.json");
            if cand.exists() {
                return cand;
            }
        }
    }
    // ① 实现方式：优先级 2——可执行文件同目录下若**已存在** key-store.json 就用它。
    // ② 关键程序解析：用 exists() 判存在，所以只有"便携版预先放了文件"才命中；不存在则继续往下。
    //    注意这会让 store 落到 exe 目录（如 bin/）而非 APPDATA——是文档里写明的既有优先级设计，
    //    但 save() 也会写到这里，部署时需确保该目录不会被打进发布包/同步盘。
    // ③ 功能：支持便携版（密钥随 exe 走）。
    let mut base = {
        #[cfg(windows)]
        {
            PathBuf::from(std::env::var("APPDATA").unwrap_or_else(|_| ".".into()))
        }
        #[cfg(not(windows))]
        {
            match std::env::var("XDG_CONFIG_HOME") {
                Ok(x) if !x.trim().is_empty() => PathBuf::from(x),
                _ => {
                    let mut h = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                    h.push_str("/.config");
                    PathBuf::from(h)
                }
            }
        }
    };
    // ① 实现方式：优先级 3 的基目录——Windows 取 %APPDATA%；Unix 取 XDG_CONFIG_HOME，缺省退 $HOME/.config。
    // ② 关键程序解析：cfg 条件编译分平台；环境变量缺失时 unwrap_or_else 退化成 "."（当前目录），
    //    这是兜底而非正常路径——正常 Windows 登录会话一定有 APPDATA。
    //    红线要求密钥只在本机 %APPDATA%\weflow-key-provider\store.json，这条分支正是该默认位置。
    // ③ 功能：确定按平台惯例的配置根目录。
    base.push("weflow-key-provider");
    base.push("store.json");
    base
    // ① 实现方式：在基目录下拼出 weflow-key-provider/store.json 两级。
    // ② 关键程序解析：目录名与 key-provider 组件约定一致，保证多个组件读写**同一份** store；
    //    返回的是路径而非创建文件——首次 save 时才由 create_dir_all 建目录。
    // ③ 功能：产出默认密钥库文件路径（load 与 save 共用，保证读写一致）。
}

impl KeyStore {
    /// 读 store，但把「文件不存在」与「文件存在却读不出」分开：
    /// 前者 Ok(None)（首次运行属正常），后者 Err。写入方必须用这个而不是 load。
    pub fn try_load(path: &Path) -> anyhow::Result<Option<KeyStore>> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "密钥库文件存在但读不出（{:?}），拒绝按空库继续",
                    e.kind()
                ))
            }
        };
        // ① 实现方式：整文件读入；只把 ErrorKind::NotFound 归为"没有这份 store"返回 Ok(None)，
        //    其余 IO 错误（权限不足 / 被独占占用 / 目标是目录）一律 Err。
        // ② 关键程序解析：错误文案只带 ErrorKind 的 Debug 名，**不带 path** —— store 默认落在
        //    %APPDATA% 下，路径里含真实 Windows 用户名，带出去就是身份信息出境（红线）。
        // ③ 功能：让写入方能判断"可以安全新建"还是"必须停手"。
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| {
                anyhow::anyhow!(
                    "密钥库 JSON 解析失败（{:?}，第 {} 行第 {} 列），拒绝按空库继续",
                    e.classify(),
                    e.line(),
                    e.column()
                )
            })
        // ① 实现方式：反序列化成 KeyStore 后 map(Some) 包成 Option；失败时只取 classify/line/column
        //    三个元信息拼错误，不取 Display 全文。
        // ② 关键程序解析：刻意不用 `{e}` —— serde_json 的错误文本在某些类别下会回显输入片段，
        //    而输入就是明文密钥（红线：密钥绝不明文打印）。单个字段类型不符会让整份 store 解析失败，
        //    这正是以前 load 静默返回空表、随后 save 抹掉全部密钥的入口。
        // ③ 功能：把"结构读坏了"如实上报给调用方，而不是伪装成"库是空的"。
    }

    pub fn load(path: &Path) -> KeyStore {
        Self::try_load(path).ok().flatten().unwrap_or_default()
        // ① 实现方式：委托 try_load，把 Err 与 Ok(None) 统一压平成 KeyStore::default()（三张空表）。
        // ② 关键程序解析：**读取方**保留这种宽容语义是刻意的：store 坏了也不该让"打开微信库"直接崩，
        //    退化结果是回退主 key、开库失败并报错，用户能看见。但写入方绝不能用它做合并基底
        //    （那会把空表当成"磁盘上什么都没有"）——save 已改用 try_load。
        // ③ 功能：从磁盘加载密钥库，失败时降级为空库而非崩溃。
    }

    pub fn load_default() -> KeyStore {
        KeyStore::load(&store_path())
        // ① 实现方式：用 store_path() 解析出的默认路径调 load。
        // ② 关键程序解析：Account::open 与各 CLI 都走这个入口；它是一次性快照，
        //    进程运行期间别的组件写入的新密钥不会自动反映到已加载的实例。
        // ③ 功能：加载约定位置的共享密钥库。
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // ① 实现方式：先确保父目录存在（create_dir_all 幂等），失败用 `?` 冒泡。
        // ② 关键程序解析：首次写入时 weflow-key-provider 目录尚不存在，必须先建；
        //    path 无父目录（裸文件名）时 parent() 为 Some("")，create_dir_all("") 是 Ok，不影响。
        // ③ 功能：保证后续写文件不会因目录缺失而失败。
        // 先把磁盘上那份并进来再写，别整档覆盖成本进程的启动快照：
        // 入口允许 3 条链路并发，一条在扫库密钥、一条在派生图片密钥时，
        // 后存的会把先存的密钥整批抹掉（现象是"明明扫到了又没了"，还得重扫几十秒）。
        // 三个表都只增不删，所以并集是安全的；同一条目以本次（刚验证过的）为准。
        // 残留窗口从"整个进程寿命"缩到"load 与 save 之间"，真要根治得上跨进程文件锁。
        let mut merged = KeyStore::try_load(path)?.unwrap_or_default();
        // ① 实现方式：以**磁盘当前内容**为合并基底（而不是本进程的启动快照）；用 try_load 而非 load，
        //    `?` 让"文件存在却读不出"直接中止整个 save，只有 Ok(None)（文件真不存在）才起步于空表。
        // ② 关键程序解析：这一步把并发丢键窗口从"整个进程寿命"缩到"load 与 rename 之间"；
        //    改用 try_load 是为了堵掉最后一条丢键路径——以前 load 把损坏/被占用也降级成空表，
        //    merged 为空 → 只并入 self → 磁盘上其它组件刚写的密钥被整批抹掉。
        //    已用 save_refuses_to_wipe_unparsable_store 实测：改前 save 返回 Ok 并覆盖，改后返回 Err 且原文件逐字节不变。
        //    残留的并发窗口仍需跨进程文件锁才能根治，此处不引入。
        // ③ 功能：取得"磁盘已有密钥"作为并集左操作数，读不出时宁可不写也不覆盖。
        for (k, v) in &self.db_keys {
            merged.db_keys.insert(k.clone(), v.clone());
        }
        // ① 实现方式：把本进程的主密钥表逐条 insert 进 merged（同键以 self 为准）。
        // ② 关键程序解析：insert 覆盖同键，保证"刚验证过的密钥"优先于磁盘旧值；只增不删，
        //    磁盘上 self 没有的账号主密钥会被保留。
        // ③ 功能：合并主密钥表。
        for (acct, entries) in &self.db_key_maps {
            let slot = merged.db_key_maps.entry(acct.clone()).or_default();
            for (rel, key) in entries {
                slot.insert(rel.clone(), key.clone());
            }
        }
        // ① 实现方式：按账号 entry().or_default() 取（或建）内层表，再逐条 insert 相对路径→密钥。
        // ② 关键程序解析：**逐条目合并**而非整表替换——磁盘上该账号已有的其它库密钥会被保留，
        //    只有同名相对路径才以 self 为准。注意 put_db_key_map 在内存里是整表替换，
        //    若同一进程内连续两次 put 同账号且第一次未 save，第一批会在内存里丢失（save 也救不回，因为磁盘还没有）。
        // ③ 功能：合并每库密钥映射，避免覆盖掉别的链路刚扫到的库密钥。
        for (k, v) in &self.image_keys {
            merged.image_keys.insert(k.clone(), v.clone());
        }
        // ① 实现方式：图片密钥表同样逐条 insert，同键以 self 为准。
        // ② 关键程序解析：ImageKeyEntry 整体替换（不做字段级合并），所以"只更新了 xor 没带 aes"会把磁盘上的 aes 覆盖成 None；
        //    调用方需保证写入的是完整条目。
        // ③ 功能：合并图片密钥表。
        // 原子写：半截 JSON 会让下次 load 直接静默回退成空 keystore（现象=密钥全没了）。
        // tmp 名带 pid：入口并发闸允许多条链路同时跑，两条同时落钥时固定名会互相
        // 覆盖（Windows 下 rename 打开中的文件会失败）——各写各的 tmp，rename 本身原子。
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_string_pretty(&merged)?)?;
        // ① 实现方式：tmp 名 = 原扩展名替换成 `json.<pid>.tmp`（如 store.json.1234.tmp），先把合并结果
        //    pretty 序列化写入 tmp，序列化失败用 `?` 冒泡（此时不会动原文件）。
        // ② 关键程序解析：pid 后缀隔离**不同进程**的并发写；但**同一进程内多线程**共享 pid → tmp 名相同，
        //    仍会互相覆盖（当前 save 调用方都是单线程 CLI，未触发）。写 tmp 失败会留下残留文件，不清理。
        // ③ 功能：把完整 JSON 先落到临时文件，避免直接截断原文件产生半截内容。
        std::fs::rename(&tmp, path)?;
        Ok(())
        // ① 实现方式：rename(tmp → path) 原子替换原文件，成功后返回 Ok。
        // ② 关键程序解析：Windows 上 std::fs::rename 用 MOVEFILE_REPLACE_EXISTING，可覆盖已存在的目标；
        //    但若目标正被别的进程打开读取（load），可能因共享冲突失败 → `?` 冒泡，tmp 残留。
        //    rename 本身原子，所以读者要么看到旧完整文件、要么看到新完整文件，不会看到半截。
        // ③ 功能：以原子替换完成持久化，保证下次 load 不会读到损坏 JSON。
    }

    /// 解析某个库文件应使用的密钥：优先每库密钥表（按 account_dir + 相对路径），
    /// 命中即用；否则回退 `fallback`（主 session key）。
    pub fn resolve_key(&self, db: &Path, account_dir: Option<&Path>, fallback: &str) -> String {
        if let Some(dir) = account_dir {
            let acct = norm_path(dir);
            // ① 实现方式：先把 <账号目录> 用 norm_path 规整成「小写 + 正斜杠」再查
            //    db_key_maps；只有传了 account_dir 才走每库密钥这条精确路径。
            // ② 关键程序解析：微信 4.x 同一账号下 message_N.db / contact.db / sns.db 可能
            //    用不同密钥（分库分钥），所以必须先按目录定位到该账号的映射表；
            //    account_dir=None 时整段跳过，直接回退主 key（旧数据只有单一主钥）。
            // ③ 功能：把「调用方给的原始路径」翻译成「store 里登记的账号键」。
            if let Some(map) = self.db_key_maps.get(&acct) {
                if let Some(rel) = rel_under_db_storage(db, dir) {
                    if let Some(k) = map.get(&rel) {
                        return k.clone();
                    }
                }
            }
        }
        // ① 实现方式：三层 Option/嵌套 if let 全部命中才 return；任何一层落空都穿透到
        //    函数末尾的 fallback.to_string()。
        // ② 关键程序解析：rel_under_db_storage 内部用大小写敏感的 strip_prefix 匹配字面量
        //    "db_storage"（见上文注释），若磁盘上目录名大小写不同则此处返回 None → 明明
        //    登记了每库钥却用不上；k.clone() 是因为 &self 不可交出内部 String 的所有权。
        // ③ 功能：命中则返回该库专属 64hex 原始密钥，未命中则回退主 session key，
        //    保证「没导入每库表」的老账号仍能打开库。
        fallback.to_string()
    }

    /// 指定账号下所有已知密钥候选（去重）：主 key → 每库表所有值 → DEFAULT。
    /// key-provider 取 DB key 时逐个用 SQLCipher 校验，挑出能打开目标库者。
    pub fn candidate_db_keys(&self, account_id: Option<&str>, account_dir: Option<&Path>) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let push = |k: &str, out: &mut Vec<String>, seen: &mut std::collections::HashSet<String>| {
            if !k.is_empty() && seen.insert(k.to_ascii_lowercase()) {
                out.push(k.to_ascii_lowercase());
            }
        };
        // ① 实现方式：out 保序（Vec）+ seen 去重（HashSet，元素为小写 key），push 闭包
        //    用 `seen.insert(...)` 的布尔返回值做「首次出现才入列」的原子判定。
        // ② 关键程序解析：闭包不捕获环境、显式接收 &mut 两个容器，是为了绕开
        //    「同时借用 out 和 seen」的借用冲突；空串直接丢弃，避免把无效 key 送去
        //    SQLCipher 做昂贵的 PBKDF2 试错；统一 to_ascii_lowercase 保证 hex 大小写
        //    不同但值相同的 key 只试一次（PRAGMA key 的 x'..' 本身大小写不敏感）。
        // ③ 功能：构造一个「无重复、无空值、顺序稳定」的候选密钥队列。
        if let Some(dir) = account_dir {
            if let Some(map) = self.db_key_maps.get(&norm_path(dir)) {
                for k in map.values() {
                    push(k, &mut out, &mut seen);
                }
            }
        }
        // ① 实现方式：按 norm_path(<账号目录>) 取该账号的每库映射表，BTreeMap.values()
        //    按相对路径字典序遍历全部 value 并 push。
        // ② 关键程序解析：实际入列顺序是「每库表 → account_id 主 key → DEFAULT」，
        //    与上方 /// 文档写的「主 key → 每库表 → DEFAULT」不一致；因为调用方
        //    (key-provider) 是逐个真实开库校验、命中即停，顺序只影响试错耗时不影响正确性。
        //    每库表可能含几十个库的 key，故放在最前会让主 key 场景多试很多次。
        // ③ 功能：把该账号所有分库密钥纳入候选，覆盖「一账号多密钥」的情况。
        if let Some(acc) = account_id {
            if let Some(k) = self.db_keys.get(acc) {
                push(k, &mut out, &mut seen);
            }
        }
        // ① 实现方式：db_keys 以 account_id 原样字符串为键做精确 get（未做 norm）。
        // ② 关键程序解析：这里传入的是 <wxid> 或调用方自定义的账号标识，大小写/前后
        //    空格与导入时不一致就会 miss（与 db_key_maps 的 norm_path 口径不同）；
        //    miss 不报错，只是少一个候选，最终由下面的 DEFAULT 兜底。
        // ③ 功能：补入该账号的主 session 密钥。
        if let Some(k) = self.db_keys.get(DEFAULT_ACCOUNT) {
            push(k, &mut out, &mut seen);
        }
        // ① 实现方式：无条件尝试 DEFAULT_ACCOUNT（"__default__"）桶。
        // ② 关键程序解析：这是「只导入了一把通用 key、没绑定账号」时的兜底；
        //    若前面已 push 过同一把 key，seen 会拦掉，不会产生重复项。
        // ③ 功能：保证单账号/未标记账号的场景也有候选可用。
        out
        // ① 实现方式：直接返回 Vec（可能为空）。
        // ② 关键程序解析：空 Vec 表示 store 里完全没有相关密钥，调用方需自行提示
        //    「请先用 key-provider 导入」，本函数不返回 Result、不 panic。
        // ③ 功能：把候选队列交给上层做 SQLCipher 逐个验证。
    }

    pub fn image_key(&self, account_id: Option<&str>) -> Option<&ImageKeyEntry> {
        account_id
            .and_then(|a| self.image_keys.get(a))
            .or_else(|| self.image_keys.get(DEFAULT_ACCOUNT))
        // ① 实现方式：Option 链式 and_then → or_else，先按传入标识精确查 image_keys，
        //    落空再查 DEFAULT_ACCOUNT 桶；返回 Option<&ImageKeyEntry>（借用，不 clone）。
        // ② 关键程序解析：这里的键**不做任何规整**，完全依赖调用方与导入方约定同一命名：
        //    reply-exporter 用 <账号目录> 的 basename 写入/读取，key-provider 的 image 子命令
        //    用 --account 传入的 <wxid> 写入 —— 两者不一致时会 miss 并静默落到 DEFAULT；
        //    若 DEFAULT 也没有，调用方拿到 None 后 xor 密钥为 0 / AES 为 None，
        //    结果是所有 .dat 图片「解密成功但内容是乱码」或直接失败，且此处不报错。
        // ③ 功能：取出该账号的图片解密密钥条目（xor 尾钥 + 可选 AES 头钥）。
    }

    /// 该 account_dir 是否已有每库密钥映射表（诊断用）。
    pub fn has_db_key_map_for(&self, account_dir: &Path) -> bool {
        self.db_key_maps.contains_key(&norm_path(account_dir))
        // ① 实现方式：norm_path 规整后调 BTreeMap::contains_key，只看外层账号键是否存在。
        // ② 关键程序解析：返回 true 只代表「登记过这个账号」，不代表表非空、也不代表
        //    某个具体 .db 的 key 在里面；因此只能用于诊断/日志，不能用它替代 resolve_key
        //    的命中判断（空表也会 true）。
        // ③ 功能：给上层一个「该账号是否已导入每库密钥」的布尔探针。
    }

    /// 列出所有"位于 `dir` 之下（或等于 dir）"的已导入账号目录。
    /// 用于 WeFlow 只给出父级数据目录（如 <微信数据根>）时识别候选账号。
    pub fn accounts_under(&self, dir: &Path) -> Vec<PathBuf> {
        let prefix = norm_path(dir);
        self.db_key_maps
            .keys()
            .filter(|acct| *acct == &prefix || acct.starts_with(&format!("{prefix}/")))
            .map(PathBuf::from)
            .collect()
        // ① 实现方式：把父目录 norm 成 prefix，遍历所有已登记账号键，用
        //    「完全相等 || starts_with(prefix + '/')」两个条件筛选，再转回 PathBuf。
        // ② 关键程序解析：拼上末尾 '/' 是关键 —— 否则 "e:/data/wxid_1" 会错误匹配
        //    "e:/data/wxid_10"（前缀包含但不是子目录）；两侧都已 norm 成小写正斜杠，
        //    所以 Windows 大小写/反斜杠差异不会漏匹配；结果只来自 store 内存快照，
        //    不访问磁盘，磁盘上已删除的账号仍会被列出（调用方需自行 is_dir 校验）。
        // ③ 功能：当只拿到 <微信数据根> 这类父级目录时，枚举其下所有已导入密钥的账号目录。
    }

    /// 全部已导入账号目录（norm 规整后的路径）。
    pub fn all_accounts(&self) -> Vec<PathBuf> {
        self.db_key_maps.keys().map(PathBuf::from).collect()
        // ① 实现方式：直接 map BTreeMap 的 keys() → PathBuf::from → collect。
        // ② 关键程序解析：BTreeMap 保证输出按账号路径字典序稳定（便于 CLI 列表展示与
        //    diff）；返回的是 norm 后的小写正斜杠路径，**不是磁盘原始大小写**，
        //    Windows 上不区分大小写所以可直接用，但若上层要做字符串比对需注意口径；
        //    同样只反映内存快照，且只看 db_key_maps —— 只在 db_keys 里登记过主 key、
        //    没导入每库表的账号不会出现在这里。
        // ③ 功能：列出全部已导入每库密钥的账号目录。
    }

    /// 某账号目录的 session 主密钥（每库表里的 session/session.db）。
    pub fn session_key_for(&self, account_dir: &Path) -> Option<String> {
        self.db_key_maps
            .get(&norm_path(account_dir))
            .and_then(|m| m.get("session/session.db"))
            .cloned()
        // ① 实现方式：两级查表 —— 外层 norm_path(<账号目录>)，内层硬编码相对路径
        //    "session/session.db"，最后 cloned() 把 &String 变成自有 String。
        // ② 关键程序解析：内层键是**字面量正斜杠小写**，与 put_db_key_map 里 norm_rel
        //    的产物一致，所以扫描器输出的 `session\session.db` 也能命中；
        //    但只认这一个固定相对路径，若某版本微信把会话库放在别处（或 rel 登记时
        //    带了 db_storage 前缀）就返回 None，调用方须回退 db_keys 里的主 key。
        // ③ 功能：取出该账号的 session 主密钥，用于打开 session.db 并作为其他库的 fallback。
    }

    /// 把一批"每库密钥"写入某 account_dir 的映射表（相对路径键）。返回写入条数。
    pub fn put_db_key_map(&mut self, account_dir: &Path, entries: BTreeMap<String, String>) -> usize {
        let acct = norm_path(account_dir);
        let mut norm: BTreeMap<String, String> = BTreeMap::new();
        for (rel, key) in entries {
            norm.insert(norm_rel(&rel), key.trim().to_ascii_lowercase());
        }
        // ① 实现方式：账号目录 norm_path；新建一个 BTreeMap，把入参 entries 逐项
        //    「键 norm_rel（小写+正斜杠+去边界斜杠）、值 trim + to_ascii_lowercase」后插入。
        // ② 关键程序解析：用新 map 而非原地改，是为了统一清洗外部（扫描器/CLI）来的脏数据；
        //    同一次调用里若两个 rel 规整后相同（如 `message\A.db` 与 `message/A.db`），
        //    后者覆盖前者；key 只做小写化不做长度/hex 校验，非法值会被原样存进 store，
        //    直到 open_sqlcipher 的 hex 白名单才拦下。entries 按值传入（所有权转移）。
        // ③ 功能：把一批「相对 db_storage 的 .db 路径 → 64hex 原始密钥」规整成内部存储格式。
        let n = norm.len();
        self.db_key_maps.insert(acct, norm);
        n
        // ① 实现方式：先取 len 作为返回值，再 BTreeMap::insert(acct, norm) —— 这是
        //    **整表替换**语义（insert 覆盖同键的旧 value），不是逐条 merge。
        // ② 关键程序解析：n 是清洗去重后的条数，可能小于入参条数；同一进程内对同一账号
        //    连续调用两次 put_db_key_map 且中间没有 save()，第一批只存在于内存的 key 会被
        //    第二批整体覆盖并永久丢失（save 的并集合并只能救「已落盘」的键）；
        //    要增量补库需调用方自己先 load 出旧表再合并传入。
        // ③ 功能：登记/刷新该账号的每库密钥映射表，并回报写入条数供 CLI 打印。
    }

    /// 解析取钥工具的常见输出格式，逐行提取 `相对/db_storage 的 .db 路径 -> 64hex key`。
    /// 兼容形如：`  [Cipher-FOUND] message\message_4.db salt=.. -> b4a1..`。
    /// 返回 (relPath, key) 列表；无法识别的行忽略。
    pub fn parse_scanner_lines(text: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for line in text.lines() {
            let arrow = match line.find("->") {
                Some(i) => i,
                None => continue,
            };
            // ① 实现方式：lines() 按 \n / \r\n 切行；用 find("->") 定位箭头分隔符，
            //    找不到就 continue 跳过该行（噪音行、日志行一律忽略）。
            // ② 关键程序解析：find 返回的是**字节索引**，后面用它做 line[..arrow] /
            //    line[arrow+2..] 切片；因为 "->" 是 ASCII，箭头两侧即使是中文也不会
            //    切在 UTF-8 字符中间，切片安全；只取第一个箭头，若路径里本身含 "->"
            //    会截断（实际 .db 路径不会含此串）。
            // ③ 功能：把取钥工具的整段文本输出逐行过滤成「可能含 key 的行」。
            let key_raw = line[arrow + 2..].trim();
            let key: String = key_raw
                .chars()
                .take_while(|c| c.is_ascii_hexdigit())
                .collect();
            if key.len() != 64 {
                continue;
            }
            // ① 实现方式：取箭头右侧 trim 后的串，用 chars().take_while(is_ascii_hexdigit)
            //    截取**开头连续的 hex 前缀**，再要求长度恰好 64（= 32 字节 raw key）。
            // ② 关键程序解析：take_while 而非 filter 是关键 —— 遇到第一个非 hex 字符就停，
            //    这样 "<64hex> 后面跟说明文字" 或再跟 salt 时不会把杂字符混进 key；
            //    长度必须 == 64 才算 SQLCipher raw key，少于/多于都丢弃（宁可漏采不可错采）；
            //    这里不打印 key，只做长度判断，符合「绝不明文打印密钥」的红线。
            // ③ 功能：从行尾提取并校验出 64 位十六进制原始密钥。
            let left = line[..arrow].trim();
            // db 路径 = left 中最后一个以 .db 结尾的 token（前面可能带 [Cipher-FOUND] / salt 在后）
            let token = left
                .split_whitespace()
                .rev()
                .find(|t| t.to_ascii_lowercase().ends_with(".db"));
            // ① 实现方式：取箭头左侧 trim，按空白切 token 后**反向**遍历，找第一个
            //    （即原串中最后一个）以 .db 结尾的 token；后缀比较前先 to_ascii_lowercase。
            // ② 关键程序解析：反向找是为了跳过前面的 "[Cipher-FOUND]" 标记和后面可能
            //    追加的 "salt=..." 字段；但 split_whitespace 无法处理**路径中含空格**的情况
            //    （如 <账号目录> 带空格），此时只会拿到最后一段碎片 → 存进 store 的 rel
            //    与真实相对路径不符 → resolve_key 永远 miss 并静默回退主 key（见 BUG 清单）；
            //    大小写不敏感比较后缀，兼容 ".DB"。
            // ③ 功能：从格式化日志行里定位出目标库的相对路径字符串。
            let Some(tok) = token else { continue };
            let rel = tok.trim_matches(|c| c == '"' || c == '\'');
            out.push((norm_rel(rel), key.to_ascii_lowercase()));
            // ① 实现方式：let-else 在没找到 .db token 时 continue；trim_matches 剥掉 token
            //    两端成对的引号（双引号或单引号），再 norm_rel 规整路径、key 统一小写后
            //    以 (rel, key) 元组 push 进 out。
            // ② 关键程序解析：trim_matches 只剥**两端**的引号字符，路径中间的引号不动；
            //    norm_rel 会把反斜杠换成正斜杠并转小写，因此 `message\message_4.db`
            //    与 `message/message_4.db` 归一为同一键；out 不去重，同一库出现多行时
            //    会产生多条，交给 put_db_key_map 的 BTreeMap 后者覆盖前者。
            // ③ 功能：产出一条可直接写入每库密钥表的 (相对路径, 64hex key) 记录。
        }
        out
        // ① 实现方式：循环结束后返回收集到的 Vec（保持原行序）。
        // ② 关键程序解析：无法识别的行全部静默忽略、不报错也不计数，因此调用方无法
        //    区分「工具没输出」与「格式变了导致全部解析失败」，需自行比对预期库数量。
        // ③ 功能：把取钥工具文本输出转成结构化密钥清单，供 cmd_import / put_db_key_map 使用。
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norm_and_rel() {
        assert_eq!(norm_path(Path::new("E:/A/B\\")), "e:/a/b");
        // ① 实现方式：断言 norm_path 对「混合斜杠 + 末尾反斜杠 + 大写字母」的输入
        //    产出全小写、正斜杠、无尾分隔符的规范串。
        // ② 关键程序解析：末尾那个分隔符必须被去掉，否则 accounts_under 的
        //    starts_with(prefix + '/') 与 resolve_key 的查表都会因多一个 '/' 而 miss；
        //    这里用虚构盘符路径做纯字符串测试，不触碰任何真实微信目录。
        // ③ 功能：锁定账号目录/相对路径的规整口径，防止后续改动破坏 store 键的一致性。
        let dir = Path::new("E:/acct/wxid_x");
        let db = Path::new("E:/acct/wxid_x/db_storage/message/message_0.db");
        assert_eq!(rel_under_db_storage(db, dir).as_deref(), Some("message/message_0.db"));
        // ① 实现方式：构造 <账号目录> 与其下 db_storage/message/message_0.db 两个 Path，
        //    断言 rel_under_db_storage 返回去掉账号前缀和 db_storage 段后的相对路径。
        // ② 关键程序解析：as_deref() 把 Option<String> 变成 Option<&str> 才能与字面量
        //    比较；期望值不含 "db_storage/"，这正是 store 里每库表的键格式
        //    （与 session_key_for 硬编码的 "session/session.db" 同一口径）。
        // ③ 功能：验证「绝对库路径 → store 相对键」的换算正确，这是每库密钥命中的前提。
    }

    #[test]
    fn resolve_prefers_map_then_fallback() {
        let mut s = KeyStore::default();
        let mut map = BTreeMap::new();
        map.insert("message/message_0.db".to_string(), "b".repeat(64));
        s.put_db_key_map(Path::new("E:/acct/wxid_x"), map);
        // ① 实现方式：空 KeyStore + 只含一条 message/message_0.db 记录的 BTreeMap，
        //    经 put_db_key_map 写入（内部会 norm_path 账号目录、norm_rel 相对键）。
        // ② 关键程序解析：key 用 repeat(64) 合成 64 字符占位串，长度满足 raw key 口径
        //    但**不接触任何真实密钥**；此处刻意只登记一个库，好让后面的回退分支可测。
        // ③ 功能：搭出「该账号已导入每库表、但只覆盖部分库」的最小夹具。
        let dir = Path::new("E:/acct/wxid_x");
        let hit = s.resolve_key(Path::new("E:/acct/wxid_x/db_storage/message/message_0.db"), Some(dir), &"a".repeat(64));
        assert_eq!(hit, "b".repeat(64));
        // ① 实现方式：对已登记的库调 resolve_key，传入与每库钥**不同**的 fallback，
        //    断言返回的是每库表里的那把（证明优先级而非巧合相等）。
        // ② 关键程序解析：db 路径必须带 db_storage 段，rel_under_db_storage 才能剥出
        //    "message/message_0.db" 与表内键匹配；fallback 用另一合成串是为了让
        //    「命中每库钥」与「回退主 key」两种结果可区分。
        // ③ 功能：验证每库密钥优先于主 session key。
        // 未登记的库 -> 回退
        let fb = s.resolve_key(Path::new("E:/acct/wxid_x/db_storage/other/x.db"), Some(dir), &"a".repeat(64));
        assert_eq!(fb, "a".repeat(64));
        // ① 实现方式：换成表里没有的 other/x.db，断言返回值等于传入的 fallback。
        // ② 关键程序解析：三层 if let 在 map.get(&rel) 处落空后穿透到函数末尾，
        //    这条断言锁定了「未登记库不报错、静默回退」的既有行为；
        //    也正因如此，rel 规整口径一旦不一致（大小写/含空格路径）不会有任何报错，
        //    只会表现为打开库失败，属于难以察觉的失效模式。
        // ③ 功能：验证回退路径，保证老账号/新增库在没导入每库钥时仍可用主 key 尝试。
    }

    #[test]
    fn parse_scanner_output() {
        let text = r#"
  [Cipher-FOUND] message\message_4.db salt=0123 -> 1111111111111111111111111111111111111111111111111111111111111111
  [Cipher-FOUND] session\session.db salt=4567 -> 2222222222222222222222222222222222222222222222222222222222222222
some noise line
[Cipher] PID=19308: 37 keys
"#;
        let got = KeyStore::parse_scanner_lines(text);
        assert_eq!(got.len(), 2);
        // ① 实现方式：把整段夹具文本交给 parse_scanner_lines，断言只解析出 2 条记录。
        // ② 关键程序解析：夹具里故意混入「无箭头的噪音行」和「有箭头但无 64hex 的
        //    [Cipher] PID 统计行」，用来验证两类无效行都被 continue 跳过；
        //    数量断言能同时抓住「漏采」和「多采」两种回归。
        // ③ 功能：验证解析器对真实工具输出中夹杂噪音的容错能力。
        assert_eq!(got[0].0, "message/message_4.db");
        assert_eq!(got[0].1, "1111111111111111111111111111111111111111111111111111111111111111");
        // ① 实现方式：按下标取第一条，分别断言相对路径与 key 字符串。
        // ② 关键程序解析：夹具里的路径写成反斜杠 `message\message_4.db`，期望值却是
        //    正斜杠 —— 这正是 norm_rel 的规整效果，也是与 resolve_key 查表键对齐的关键；
        //    路径后面还跟着 `salt=....` 字段，验证「反向找最后一个 .db token」不会被
        //    salt 干扰；key 断言用全 1 的合成串，不涉及任何真实密钥。
        // ③ 功能：锁定「反斜杠 → 正斜杠 + 跳过 salt + 截取 64hex」三项解析行为。
        assert_eq!(got[1].0, "session/session.db");
        // ① 实现方式：断言第二条的相对路径为 session/session.db。
        // ② 关键程序解析：这个字面量与 session_key_for 内部硬编码的查表键完全一致，
        //    形成跨函数的口径闭环 —— 若哪天改动 norm_rel 或该硬编码，此断言会先失败；
        //    同时验证输出保持输入行序（message 在前、session 在后）。
        // ③ 功能：保证解析结果可直接被 session_key_for / put_db_key_map 消费。
    }

    #[test]
    fn roundtrip_save_load() {
        let path = std::env::temp_dir().join("kp_keystore_rt.json");
        let _ = std::fs::remove_file(&path);
        // ① 实现方式：在系统临时目录拼一个固定文件名作为测试用 store 路径，先
        //    remove_file 清掉可能的残留，返回值用 let _ = 显式忽略（不存在时是 Err）。
        // ② 关键程序解析：写的是 temp_dir 下的独立文件，**不是** %APPDATA% 里的真实
        //    store，符合红线；预清理很关键 —— 现在的 save() 会先读盘做并集合并，
        //    若残留文件里有别的账号键会被一起并进来，使断言不再等价于「纯 roundtrip」；
        //    文件名固定意味着两个测试进程并发跑会互相覆盖（tmp 只带 pid、目标名不带）。
        // ③ 功能：准备一个干净、隔离、可反复使用的落盘目标。
        let mut s = KeyStore::default();
        let mut map = BTreeMap::new();
        map.insert("sns/sns.db".to_string(), "c".repeat(64));
        s.put_db_key_map(Path::new("E:/acct/a"), map);
        s.db_keys.insert(DEFAULT_ACCOUNT.into(), "d".repeat(64));
        // ① 实现方式：空 store 里同时塞两类数据 —— 经 put_db_key_map 写入的每库表
        //    （sns/sns.db），以及直接 insert 到 db_keys 的 DEFAULT 主 key。
        // ② 关键程序解析：故意覆盖两个不同的 BTreeMap 字段，验证 serde 序列化不会
        //    漏掉任一张表；key 全用 repeat(64) 合成串，路径为虚构账号目录，
        //    不出现真实 <wxid> 或真实密钥。
        // ③ 功能：构造含「每库钥 + 默认主钥」的最小完整 store。
        s.save(&path).unwrap();
        let loaded = KeyStore::load(&path);
        assert!(loaded.db_key_maps.contains_key(&norm_path(Path::new("E:/acct/a"))));
        assert_eq!(loaded.db_keys.get(DEFAULT_ACCOUNT).unwrap(), &"d".repeat(64));
        // ① 实现方式：save() 走「读盘并集合并 → pid 后缀 tmp → rename」原子写；
        //    再 load() 读回，分别断言账号键存在与 DEFAULT 主 key 值相等。
        // ② 关键程序解析：save 用 unwrap() 让写失败直接 panic（测试里不容忍静默失败），
        //    而 load 内部对任何错误都返回 default，所以若 JSON 写坏，断言会以
        //    「键不存在」的形式失败而不是报 IO 错；contains_key 前先 norm_path，
        //    与写入时的规整口径保持一致；这里只验证键存在，未校验 sns 那把 key 的值。
        // ③ 功能：验证 store 的序列化/反序列化与原子落盘链路可往返，两张表都不丢。
        let _ = std::fs::remove_file(&path);
        // ① 实现方式：测试结尾再次 remove_file 清理临时产物，忽略结果。
        // ② 关键程序解析：清理放在断言之后 —— 若前面断言失败会 panic 而跳过清理，
        //    残留文件会影响下次运行（但开头的预清理能兜住）；不删 pid 后缀 tmp，
        //    正常路径下它已被 rename 掉，只有 rename 失败时才会留下残留文件。
        // ③ 功能：避免测试污染临时目录、保证可重复执行。
    }

    #[test]
    fn save_refuses_to_wipe_unparsable_store() {
        let path = std::env::temp_dir().join(format!("kp_keystore_corrupt_{}.json", std::process::id()));
        let broken = r#"{"db_keys":{"__default__":"eeeeeeee"},"db_key_maps":{"e:/acct/a""#;
        std::fs::write(&path, broken).unwrap();
        // ① 实现方式：在 temp_dir 下建一个带 pid 的独立文件，写入**半截 JSON**（缺右花括号）模拟
        //    上次写到一半掉电 / 被别的进程截断的真实场景。
        // ② 关键程序解析：pid 后缀让本测试与其它测试、并发测试进程互不干扰；夹具里带一条
        //    __default__ 主 key 和一个账号目录，代表"磁盘上本来有别的组件的密钥"。
        //    合成短串而非 64hex，因为这里只测 JSON 解析失败路径，不做 SQLCipher 校验。
        // ③ 功能：造出"文件存在但读不出结构"的故障输入。
        let mut s = KeyStore::default();
        s.image_keys.insert(
            "wxid_probe".into(),
            ImageKeyEntry { xor_key: 7, aes_key: None },
        );
        let r = s.save(&path);
        // ① 实现方式：本进程只带一条图片密钥（不含磁盘上那条主 key），对损坏文件调 save。
        // ② 关键程序解析：save 内部以 KeyStore::load(path) 作合并基底，而 load 对解析失败
        //    返回 default()（空表）——若 save 不区分"文件不存在"与"文件读坏了"，
        //    就会拿空表并上 self 再 rename 覆盖，磁盘上原有密钥被静默抹掉。
        // ③ 功能：触发待验证的丢键路径。
        assert!(r.is_err(), "读不出原 store 时 save 必须报错而不是覆盖");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken, "原文件必须逐字节保持不变，便于人工抢救");
        // ① 实现方式：两条断言——save 返回 Err，且磁盘内容与写入的 broken 串逐字节相等。
        // ② 关键程序解析：只断言 Err 不够，还要确认它是**在 rename 之前**失败的；
        //    若实现先覆盖再报错，第一条可能过而第二条会挂。保留原文件是刻意的：
        //    半截 JSON 里的密钥仍可人工提取，覆盖掉就真没了。
        // ③ 功能：把"损坏 store 不得被覆盖"钉成回归。
        let _ = std::fs::remove_file(&path);
        // ① 实现方式：清理本测试的临时文件，忽略结果。
        // ② 关键程序解析：断言失败时会跳过这一步，但文件名带 pid，下次运行是新名字，不会串味。
        // ③ 功能：不污染临时目录。
    }
}
