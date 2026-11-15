//! 微信 .dat 媒体解密/重加密。
//!
//! 严格复刻 WeFlow `electron/services/imageDecryptService.ts` 中 JS 参考实现的行为：
//! - V1（含未识别版本）：全文件逐字节 XOR；
//! - V2：15 字节头（6 字节 magic + aesSize:i32le@6 + xorSize:i32le@10 + flag:u8@14），
//!   payload = AES-128-ECB(PKCS7) 前段 + raw 中段 + XOR 尾段。

use aes::cipher::{generic_array::GenericArray, BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use anyhow::{anyhow, bail, Result};

pub const SIG_V1: [u8; 6] = [0x07, 0x08, 0x56, 0x31, 0x08, 0x07];
pub const SIG_V2: [u8; 6] = [0x07, 0x08, 0x56, 0x32, 0x08, 0x07];
const V2_HEADER_LEN: usize = 0x0f;
// ① 实现方式：两个 6 字节 magic 常量，只有第 4 字节不同（0x31='1' / 0x32='2'，即 ASCII 版本号）；
//    V2_HEADER_LEN = 0x0f = 15，布局为 magic[0..6] + aesSize:i32le[6..10] + xorSize:i32le[10..14] + flag:u8[14]。
// ② 关键程序解析：magic 是**加密容器自己的头**，不是图片格式头，所以 detect_ext 认不出它；
//    长度字段用小端 i32 写入、代码按 u32 读出（微信不会写负数，故等价）；15 字节头意味着
//    payload 从偏移 15 开始，AES 段的 16 字节对齐是相对 payload 而非整个文件算的。
//    没有 SIG_V3 常量——lib.rs 的 //! 提到 V3，但本文件只实现 V1/V2，V3 会落到"全文件 XOR"兜底。
// ③ 功能：给版本识别与头解析提供唯一的字节布局真值。

/// 与 TS `NativeDatMeta` 对应（nativeImageDecrypt.ts 同时接受 camel/snake，
/// 上层输出统一为 snake_case）。
#[derive(Debug, Clone, Default)]
pub struct DatMeta {
    pub version: Option<u32>,
    pub aes_size: Option<u32>,
    pub xor_size: Option<u32>,
    pub raw_size: Option<u32>,
    pub flag: Option<u32>,
    // ① 实现方式：五个 Option<u32> 字段，Default 全 None；解密时填充、重加密时作为可选入参回读。
    // ② 关键程序解析：全用 Option 是为了区分"没提供"与"提供了 0"——encrypt_dat 里
    //    aes_size=None 走 1023 缺省、=Some(0) 则真的不加密任何前段；xor_size 同理。
    //    raw_size 只在解密出参里有意义（中段明文长度），重加密时由 plain.len()-aes_size-xor_size 推回，不读它。
    //    flag 是 u8 语义却存成 u32，写回时 `as u8` 截断（解密读出的值本就 ≤255，无损）。
    // ③ 功能：在解密/重加密之间无损传递 V2 容器的分段参数，保证 roundtrip 字节一致。
}

#[derive(Debug, Clone)]
pub struct DecryptedDat {
    pub data: Vec<u8>,
    /// 不含点号的小写扩展名；WXGF 容器为 "wxgf"。
    pub ext: String,
    pub is_wxgf: bool,
    pub version: u32,
    pub meta: DatMeta,
    // ① 实现方式：解密出参结构体——明文字节 + 识别出的扩展名 + WXGF 标记 + 版本 + 分段元数据。
    // ② 关键程序解析：version 语义是 0=明文直通（未加密）、1=V1/未识别版本走全文件 XOR、2=V2；
    //    is_wxgf 与 ext=="wxgf" 冗余但必须同时给（上层 TS 两个字段都读）；WXGF 是微信自有容器，
    //    解出它**不等于**拿到可直接显示的图片，还需要后续转换。data 是完整明文（可能几 MB），
    //    所以本结构不做 Copy，调用方应尽量移动而非克隆。
    // ③ 功能：把"解密结果 + 它是什么格式 + 怎么解出来的"一次性交给上层。
}

/// 对应 TS `getDatVersion`：0=未知 1=V1 2=V2。
pub fn dat_version(data: &[u8]) -> u32 {
    if data.len() < 6 {
        return 0;
    }
    // ① 实现方式：先做长度下界检查，不足 6 字节直接判 0（未知）。
    // ② 关键程序解析：这一步是后面 `data[..6]` 切片的**唯一**越界防线，必须先于任何索引；
    //    0 在下游不等于"明文"，而是"没识别出容器版本"，会走 V1 全文件 XOR 兜底。
    // ③ 功能：防止对截断/空文件做切片 panic。
    if data[..6] == SIG_V1 {
        1
    } else if data[..6] == SIG_V2 {
        2
    } else {
        0
    }
    // ① 实现方式：把前 6 字节与两个 magic 常量逐字节比较，命中返回 1/2，都不中返回 0。
    // ② 关键程序解析：分支顺序 V1 先于 V2（两者只差第 4 字节，不会互相误命中）；
    //    返回 0 的三类文件——明文图片、V3/未来版本容器、纯噪声——在这里无法区分，
    //    区分工作交给 decrypt_dat：先看 detect_ext 是否认得明文，认不得才当 V1 加密容器处理。
    // ③ 功能：给出 .dat 容器版本号，供上层选择解密路径与填 meta.version。
}

/// 对应 TS `detectImageExtension`，并补充 native 侧需要的 mp4/wxgf 识别。
pub fn detect_ext(data: &[u8]) -> Option<&'static str> {
    if data.len() < 12 {
        return None;
    }
    // ① 实现方式：统一长度下界 12 字节，不足直接 None。
    // ② 关键程序解析：12 是为了让后面 `data[8..12]`（WEBP）与 `data[4..8]`（ftyp）两处切片都安全；
    //    副作用是**小于 12 字节的合法小图**（如极小 BMP/GIF）一律识别失败，
    //    在 decrypt_dat 里会表现为"解密后无法识别格式"而报错。
    // ③ 功能：一次性守住所有魔数偏移的越界风险。
    if data.starts_with(&[0x77, 0x78, 0x67, 0x66]) {
        return Some("wxgf");
    }
    // ① 实现方式：前 4 字节 == ASCII "wxgf" → 判为微信自有 WXGF 容器。
    // ② 关键程序解析：必须排在图片魔数之前先判（它不是标准图片格式）；命中后 is_wxgf=true，
    //    上层知道还要再做一次 WXGF→图片转换，不能直接当 jpg 送进 LLM/企微。
    // ③ 功能：识别 WXGF（微信动图/表情新容器）。
    if data.starts_with(&[0x47, 0x49, 0x46]) {
        return Some("gif");
    }
    if data.starts_with(&[0x89, 0x50, 0x4e, 0x47]) {
        return Some("png");
    }
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("jpg");
    }
    // ① 实现方式：三条 starts_with 魔数比较——"GIF"、PNG 的 89 50 4E 47、JPEG 的 FF D8 FF。
    // ② 关键程序解析：都只读前 3~4 字节，是**最弱**的格式判定（不解析后续结构），
    //    因此 XOR 密钥错误时理论上可能碰巧撞上某个魔数而误判；实测概率极低但非零。
    //    JPEG 只验 3 字节不验第 4 字节（FFE0/FFE1 都算），以兼容各种 EXIF/APPn 变体。
    // ③ 功能：识别三种最常见的静态图/动图格式。
    if data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        return Some("webp");
    }
    // ① 实现方式：双条件——前 4 字节 "RIFF" 且偏移 8..12 为 "WEBP"。
    // ② 关键程序解析：只判 RIFF 会把 WAV/AVI 也认成 webp，所以必须再验 form type；
    //    中间的 4..8 是文件长度字段，跳过不看。这里正是需要 len>=12 的原因。
    // ③ 功能：识别 WebP（微信静态图常用档位）。
    if data.starts_with(b"BM") {
        return Some("bmp");
    }
    // ① 实现方式：前 2 字节 "BM" → BMP。
    // ② 关键程序解析：BMP 魔数只有 2 字节，是所有分支里**最易误命中**的一条，
    //    因此它被排在 png/jpg/webp 之后，尽量让更强的魔数先命中。
    // ③ 功能：识别 BMP。
    if &data[4..8] == b"ftyp" {
        return Some("mp4");
    }
    // ① 实现方式：ISO-BMFF 的 box type 字段固定在偏移 4..8，等于 "ftyp" 就判 mp4。
    // ② 关键程序解析：**不看前 4 字节的 box size、也不看 major brand**，所以 mov/3gp/m4a/heic
    //    等同样基于 ftyp 的容器都会被标成 "mp4"；对"视频 vs 音频"的区分能力为零，
    //    上层若按 ext 决定 MIME 需要自己再判。这是刻意的宽松识别（native 侧只需知道"是媒体"）。
    // ③ 功能：识别 MP4 家族容器（视频消息/部分表情）。
    None
    // ① 实现方式：所有魔数都不中 → None。
    // ② 关键程序解析：None 有两种含义——"还是加密状态"或"密钥错/格式不支持"，本函数无法区分；
    //    decrypt_dat 依赖它做两次判定（解密前判明文直通、解密后判是否成功），两次都 None 就报错回退。
    // ③ 功能：给出"不认识"的统一信号。
}

fn ecb_decrypt(key16: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key16));
    let mut out = data.to_vec();
    // ① 实现方式：用 aes crate 的 Aes128 建 ECB 分组密码，先把输入整块克隆成可写缓冲。
    // ② 关键程序解析：密钥类型是 &[u8;16]（编译期保证 16 字节），from_slice 不会失败；
    //    ECB 无 IV、无链接，每块独立，所以可以原地逐块解密、结果与顺序无关。
    // ③ 功能：准备解密上下文与输出缓冲。
    for chunk in out.chunks_mut(16) {
        cipher.decrypt_block(GenericArray::from_mut_slice(chunk));
    }
    // ① 实现方式：chunks_mut(16) 切成 16 字节块，逐块调 decrypt_block 原地解密。
    // ② 关键程序解析：**from_mut_slice 要求切片长度恰好 16，否则 panic**——本函数没有长度校验，
    //    安全性完全依赖调用方传入 16 的整数倍（decrypt_dat 传的是 aligned、encrypt 侧传的是 add_pkcs7 结果）。
    //    ECB 不去 padding，PKCS7 由 strip_pkcs7_strict 单独处理；不做任何认证（无 MAC），
    //    因此错误密钥不会在这里报错，只会产出乱码，最终靠 detect_ext 兜底判失败。
    // ③ 功能：完成 AES-128-ECB 解密（V2 容器前段）。
    out
    // ① 实现方式：返回解密后的缓冲（长度与输入相同，仍含 PKCS7 填充）。
    // ② 关键程序解析：调用方必须紧接着 strip padding，否则明文尾部会多出 1..16 个填充字节。
    // ③ 功能：交付未去填充的明文块。
}

fn ecb_encrypt(key16: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key16));
    let mut out = data.to_vec();
    for chunk in out.chunks_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
    }
    // ① 实现方式：与 ecb_decrypt 完全对称，只是把 decrypt_block 换成 encrypt_block。
    // ② 关键程序解析：同样要求入参长度是 16 的整数倍（调用方先 add_pkcs7），否则 from_mut_slice panic；
    //    这条路径只服务 encrypt_dat（编辑/重加密场景），只读导出链路不会走到。
    // ③ 功能：完成 AES-128-ECB 加密，保证与解密侧字节级互逆。
    out
    // ① 实现方式：返回密文缓冲。
    // ② 关键程序解析：长度 = 明文长 + PKCS7 填充 = decrypt 侧算出的 aligned，两侧公式必须一致才能 roundtrip。
    // ③ 功能：交付 V2 容器的 AES 段密文。
}

fn strip_pkcs7_strict(data: &[u8]) -> Result<Vec<u8>> {
    let n = data.len();
    if n == 0 || n % 16 != 0 {
        bail!("pkcs7: invalid length {n}");
    }
    // ① 实现方式：先校验总长度非零且为 16 的整数倍，不满足直接 bail。
    // ② 关键程序解析：这是"严格"模式的第一道闸——AES-ECB 输出必然是块对齐的，
    //    不对齐说明上游 aes_size 算错或文件被截断，此时继续解 padding 只会得到看似合法的错误结果。
    // ③ 功能：拒绝结构上不可能的输入。
    let pad = data[n - 1] as usize;
    if pad == 0 || pad > 16 || pad > n {
        bail!("pkcs7: invalid padding byte {pad}");
    }
    // ① 实现方式：取最后一字节作为填充长度 pad，校验 1<=pad<=16 且 pad<=n。
    // ② 关键程序解析：PKCS7 规定 pad 必须在 1..=块大小之间（**0 非法**，整块填充时 pad=16）；
    //    pad>n 会让下面的 `data[n-pad..]` 下溢 panic，所以这条边界必须先挡住。
    //    错误密钥解出的随机字节有约 1/16 概率通过这一关，因此还需要下一道全字节比对。
    // ③ 功能：校验填充长度字段的合法性。
    if data[n - pad..].iter().any(|&b| b as usize != pad) {
        bail!("pkcs7: padding bytes mismatch");
    }
    // ① 实现方式：检查尾部 pad 个字节是否**全部**等于 pad，任一不符即 bail。
    // ② 关键程序解析：这才是真正拦住错误密钥/损坏文件的一关（随机字节全等的概率约 2^-8(pad-1)）；
    //    严格校验的代价是：若微信某些 V2 文件的 aesSize 与真实 PKCS7 边界不一致，整文件解密会直接失败，
    //    上层据此回退到 JS 解密路径，而不是产出一张尾部带垃圾的图。
    // ③ 功能：确认填充完整一致。
    Ok(data[..n - pad].to_vec())
    // ① 实现方式：切掉尾部 pad 字节后克隆返回。
    // ② 关键程序解析：pad 最大 16，故输出长度 = n-pad，可能比 n 少整整一个块；
    //    返回自有 Vec 而非切片，便于与 raw/xor 段拼接成完整明文。
    // ③ 功能：交付去填充后的 AES 段明文。
}

fn add_pkcs7(data: &[u8]) -> Vec<u8> {
    let pad = 16 - (data.len() % 16);
    let mut out = data.to_vec();
    out.extend(std::iter::repeat(pad as u8).take(pad));
    // ① 实现方式：pad = 16 - (len % 16)，克隆输入后追加 pad 个值为 pad 的字节。
    // ② 关键程序解析：len 已是 16 倍数时 pad=16（**必须补一整块**，不能补 0），
    //    这与 decrypt 侧 `aligned = aes_size + (16 - aes_size % 16)` 的公式严格对偶；
    //    pad 范围恒为 1..=16，`as u8` 无截断风险。
    // ③ 功能：把任意长度明文补齐到块边界，供 ECB 加密。
    out
    // ① 实现方式：返回补齐后的缓冲。
    // ② 关键程序解析：输出长度恒为 16 的整数倍，正好满足 ecb_encrypt 的 chunks_mut(16) 前提。
    // ③ 功能：交付可直接加密的明文块。
}

fn xor_bytes(data: &[u8], key: u8) -> Vec<u8> {
    data.iter().map(|b| b ^ key).collect()
    // ① 实现方式：逐字节与单字节 key 异或后 collect 成新 Vec（不是原地修改）。
    // ② 关键程序解析：XOR 自逆，同一函数同时用于 V1 解密与重加密；key 已是 u8，
    //    调用方负责 `xor_key & 0xff` 截断（与 JS 写入 Buffer 时的截断行为一致）。
    //    全文件 XOR 无头部、无长度字段，所以密钥错了不会报错，只会让 detect_ext 认不出格式。
    // ③ 功能：实现 V1 全文件加密/解密与 V2 尾段加密/解密。
}

/// aesKey 为 ASCII 字符串，取前 16 字节（与 TS `Buffer.from(aesKeyText,'ascii').subarray(0,16)` 一致）。
fn normalize_aes_key(aes_key: Option<&str>) -> Option<[u8; 16]> {
    let text = aes_key?.trim();
    // ① 实现方式：Option 用 `?` 提前返回 None，再 trim 掉首尾空白。
    // ② 关键程序解析：None（未提供密钥）与 Some("")（提供了空串）都会走到下面的长度判定而返回 None，
    //    两者在调用方看来等价——V2 无合法密钥即失败，不做猜测。
    // ③ 功能：把可选密钥文本收敛成可判长度的字符串。
    if text.len() < 16 {
        return None;
    }
    // ① 实现方式：字节长度不足 16 直接 None。
    // ② 关键程序解析：`len()` 是**字节数**不是字符数，与下面 as_bytes()[..16] 的口径一致；
    //    要求"至少 16"而非"恰好 16"，因为上层派生出的 aes 是 md5 十六进制串的前 16 字符（长度正好 16），
    //    而 TS 侧允许传更长的文本再截断。
    // ③ 功能：拒绝短密钥（短密钥在 TS 侧同样不产生候选）。
    let mut key = [0u8; 16];
    key.copy_from_slice(&text.as_bytes()[..16]);
    Some(key)
    // ① 实现方式：取前 16 个**字节**拷进定长数组 [u8;16] 返回。
    // ② 关键程序解析：按字节切而非按字符切，含多字节 UTF-8 的文本会被切在字符中间——
    //    这是刻意的，为了与 TS 的 Buffer.subarray(0,16) 字节语义完全一致；
    //    返回定长数组让 Aes128::new 在类型层面就不可能拿到错误长度的密钥。
    // ③ 功能：产出 AES-128 可用的 16 字节密钥。
}

/// 解密 dat 字节。`xor_key` 只使用低 8 位（与 JS `data[i] ^ xorKey` 写入 Buffer 的截断一致）。
///
/// 行为与 WeFlow fallback 对齐：
/// - 未加密文件（直接可识别扩展名）原样返回，version=0；
/// - V2 且提供了合法 aesKey 才走 AES 路径，否则视为失败（TS 中 V2 无 aesKey 不产生候选）；
/// - 解密结果无法识别为任何媒体格式时返回 Err（让上层回退到 JS 解密路径）。
pub fn decrypt_dat(data: &[u8], xor_key: u32, aes_key: Option<&str>) -> Result<DecryptedDat> {
    let xk = (xor_key & 0xff) as u8;
    // ① 实现方式：入参 xor_key 是 u32，先 & 0xff 再 as u8，只保留低 8 位。
    // ② 关键程序解析：这一步复刻 JS 侧 `data[i] ^ xorKey` 写回 Buffer 时的隐式截断；
    //    上层从 keystore 读到的 xor_key 是 u64（十进制 code 派生），不截断会与 TS 结果不一致。
    // ③ 功能：统一 XOR 单字节密钥口径。

    if let Some(ext) = detect_ext(data) {
        let is_wxgf = ext == "wxgf";
        return Ok(DecryptedDat {
            data: data.to_vec(),
            ext: ext.to_string(),
            is_wxgf,
            version: 0,
            meta: DatMeta { version: Some(0), ..Default::default() },
        });
    }
    // ① 实现方式：**先对原始字节试一次格式识别**，认得就说明文件根本没加密，原样克隆返回，version=0。
    // ② 关键程序解析：这个分支必须在版本判定之前——微信部分 .dat（尤其新版/已另存的）就是明文图片；
    //    version=0 与 meta.version=Some(0) 同时给出，上层据此跳过重加密。
    //    代价：若某个 V1 密文的前几字节恰好撞上魔数（例如 xor_key=0），会被误判成明文直通。
    // ③ 功能：明文直通，避免对未加密文件做无谓 XOR 而破坏内容。

    let version = dat_version(data);
    if version == 2 {
        let Some(key16) = normalize_aes_key(aes_key) else {
            bail!("dat v2 requires a 16+ char aesKey");
        };
        // ① 实现方式：识别容器版本；V2 分支先用 let-else 强制要求合法 aesKey，拿不到就 bail。
        // ② 关键程序解析：与 TS 行为对齐——V2 无 aesKey 时**不产生候选**（而不是降级成 XOR），
        //    因为 V2 前段是 AES，缺密钥必然解不出；错误串只说长度要求，不回显任何密钥内容。
        // ③ 功能：选择解密路径并守住 V2 的密钥前提。
        if data.len() < V2_HEADER_LEN {
            bail!("dat v2 file too small");
        }
        // ① 实现方式：长度必须 >= 15 字节头，否则 bail。
        // ② 关键程序解析：dat_version 只保证 >=6 字节，所以这里还要再挡一次；
        //    不挡住的话下面 header[6..10]/[10..14]/[14] 三处切片都会 panic。
        // ③ 功能：防截断 V2 文件导致越界。
        let header = &data[..V2_HEADER_LEN];
        let aes_size = u32::from_le_bytes(header[6..10].try_into()?) as usize;
        let xor_size = u32::from_le_bytes(header[10..14].try_into()?) as usize;
        let flag = header[14] as u32;
        let payload = &data[V2_HEADER_LEN..];
        // ① 实现方式：按固定偏移小端解析——[6..10]=aesSize、[10..14]=xorSize、[14]=flag，其余为 payload。
        // ② 关键程序解析：try_into()? 把 &[u8] 转成 [u8;4]（长度已由切片保证，理论上不会失败）；
        //    读成 u32 后转 usize，在 64 位目标上不会溢出，但**aes_size 完全来自文件、不可信**，
        //    所以下面每一步都要与 payload 实际长度比对；flag 只透传不参与解密计算。
        // ③ 功能：拆出 V2 三段（AES 段 / raw 段 / XOR 段）的边界参数。

        let aligned = aes_size + (16 - aes_size % 16);
        if aligned > payload.len() {
            bail!("invalid aes size: {aes_size} (payload {})", payload.len());
        }
        // ① 实现方式：把 aes_size 向上取整到 16 的倍数得到 aligned（含 PKCS7 那一块），再与 payload 长度比对。
        // ② 关键程序解析：aes_size 是**明文中被 AES 加密的字节数**，密文因填充必然比它长 1..16 字节，
        //    公式与 add_pkcs7 严格对偶；aes_size=0 时 aligned=16（纯填充块），aes_size 为 16 倍数时也要多算一块。
        //    这道检查防止 `payload[..aligned]` 越界，也挡住伪造/损坏头导致的巨值。
        // ③ 功能：确定 AES 段密文的真实长度并做边界校验。
        let plain_aes = strip_pkcs7_strict(&ecb_decrypt(&key16, &payload[..aligned]))?;
        // ① 实现方式：对 payload 前 aligned 字节做 ECB 解密，再严格去 PKCS7，得到 AES 段明文。
        // ② 关键程序解析：两步都可能 bail（填充长度非法/填充字节不一致），`?` 直接冒泡；
        //    这是错误 aesKey 的主要暴露点——ECB 本身不报错，全靠 PKCS7 严格校验发现密钥不对。
        // ③ 功能：还原 V2 前段明文。

        let remaining = &payload[aligned..];
        if xor_size > remaining.len() {
            bail!("invalid xor size: {xor_size} (remaining {})", remaining.len());
        }
        let raw_len = remaining.len() - xor_size;
        // ① 实现方式：AES 段之后的剩余字节 = raw 中段 + XOR 尾段；用 xor_size 反推 raw_len。
        // ② 关键程序解析：先校验 xor_size <= remaining.len() 再做减法，避免 usize 下溢 panic；
        //    raw_len 可以为 0（整个剩余部分都是 XOR 尾段），此时三段拼接依然成立。
        // ③ 功能：确定中段与尾段的分界。

        let mut out = Vec::with_capacity(plain_aes.len() + remaining.len());
        out.extend_from_slice(&plain_aes);
        out.extend_from_slice(&remaining[..raw_len]);
        out.extend_from_slice(&xor_bytes(&remaining[raw_len..], xk));
        // ① 实现方式：按「AES 明文 + raw 原样 + XOR 尾段解密」顺序拼成完整明文，预分配容量避免多次扩容。
        // ② 关键程序解析：容量用 plain_aes.len()+remaining.len() 是**上界**（去填充后会更短），只影响内存不影响正确性；
        //    中段必须原样拷贝（它本来就没加密），尾段用同一个 xk 异或；三段顺序错了图片头就会错位、detect_ext 失败。
        // ③ 功能：还原整张图片/视频的明文字节。

        let ext = detect_ext(&out).ok_or_else(|| anyhow!("unrecognized media format after decrypt"))?;
        let is_wxgf = ext == "wxgf";
        // ① 实现方式：对拼接结果再做一次魔数识别，None 则转成 anyhow 错误返回。
        // ② 关键程序解析：这是解密成功的**最终判据**（PKCS7 通过仍可能密钥错，概率约 1/256）；
        //    失败时返回 Err 让上层回退到 JS 解密路径，而不是把乱码当图片输出。
        // ③ 功能：确认解出来的确实是媒体格式。
        return Ok(DecryptedDat {
            data: out,
            ext: ext.to_string(),
            is_wxgf,
            version: 2,
            meta: DatMeta {
                version: Some(2),
                aes_size: Some(aes_size as u32),
                xor_size: Some(xor_size as u32),
                raw_size: Some(raw_len as u32),
                flag: Some(flag),
            },
        });
        // ① 实现方式：打包明文 + 扩展名 + version=2 + 完整 meta（含推导出的 raw_size）返回。
        // ② 关键程序解析：meta 里的 aes_size/xor_size 直接回写文件头里的原值，raw_size 是本次算出来的，
        //    三者一起才能让 encrypt_dat 复现出字节一致的容器；usize→u32 的 as 转换在超过 4GB 时会截断
        //    （微信媒体远小于此，实际不触发）。
        // ③ 功能：交付 V2 解密结果与可逆重加密所需的全部参数。
    }

    // V1 / 未识别版本：全文件 XOR。能走到这里说明原始字节不是可识别图片
    // （明文直通分支已在前面返回），因此这就是一个 V1 加密容器，版本记为 1。
    let out = xor_bytes(data, xk);
    // ① 实现方式：对整个文件（含任何头部）逐字节 XOR，不跳过任何偏移。
    // ② 关键程序解析：version==0（未识别 magic）与 version==1 走的是**同一条**路径，
    //    因为 V1 容器没有独立头部、magic 本身也是被 XOR 过的；未来的 V3 若魔数不同也会落到这里，
    //    XOR 后 detect_ext 认不出就报错，不会静默产出错误内容。
    // ③ 功能：还原 V1（全文件 XOR）加密的媒体。
    let ext = detect_ext(&out).ok_or_else(|| anyhow!("unrecognized media format after decrypt"))?;
    let is_wxgf = ext == "wxgf";
    let v1_version = if version == 0 { 1 } else { version };
    // ① 实现方式：同样以 detect_ext 作为成功判据；再把 version=0 归一成 1 写入出参。
    // ② 关键程序解析：归一的意义是"对外只承认 0=明文/1=V1/2=V2"，避免上层看到 0 却拿到了解密结果而困惑；
    //    失败即 Err，让上层回退 JS 路径（例如 xor_key 取错的情况）。
    // ③ 功能：确认 V1 解密成功并统一版本口径。
    Ok(DecryptedDat {
        data: out,
        ext: ext.to_string(),
        is_wxgf,
        version: v1_version,
        meta: DatMeta {
            version: Some(v1_version),
            aes_size: Some(0),
            xor_size: Some(data.len() as u32),
            raw_size: Some(0),
            flag: None,
        },
    })
    // ① 实现方式：打包 V1 结果，meta 里 aes_size=0、xor_size=整个文件长度、raw_size=0、flag=None。
    // ② 关键程序解析：这组 meta 让 encrypt_dat 在 version=1 时直接全文件 XOR 复现原容器；
    //    flag=None 表示 V1 没有 flag 字段（不是 0），重加密时也不会凭空写一个头。
    // ③ 功能：交付 V1 解密结果。
}

/// 重新加密（编辑场景，对应 `encryptDatNative`）。
///
/// - version 取 `meta.version`，缺省时：有 aesKey → 2，否则 → 1；
/// - V2：aes_size 取 `meta.aes_size`（缺省 1023，并夹到明文长度），xor_size 取
///   `meta.xor_size`（缺省 0），flag 取 `meta.flag`（缺省 0）。
pub fn encrypt_dat(
    plain: &[u8],
    xor_key: u32,
    aes_key: Option<&str>,
    meta: Option<&DatMeta>,
) -> Result<Vec<u8>> {
    let xk = (xor_key & 0xff) as u8;
    let key16 = normalize_aes_key(aes_key);
    let version = meta
        .and_then(|m| m.version)
        .unwrap_or(if key16.is_some() { 2 } else { 1 });
    // ① 实现方式：同样先截断 XOR 密钥、规范化 AES 密钥；版本优先取 meta.version，
    //    缺省时按"有合法 aesKey → 2，否则 → 1"推断。
    // ② 关键程序解析：and_then 而不是 map，因为 meta 本身是 Option、version 又是 Option（双层展平）；
    //    推断规则保证无密钥时绝不会误走 V2 路径。注意 meta.version 若是 3 或其它值，
    //    下面 `version != 2` 会把它当 V1 全文件 XOR 处理——V3 未实现，重加密会产出错误容器（只读链路不调用本函数）。
    // ③ 功能：确定重加密要生成哪种容器。

    if version != 2 {
        return Ok(xor_bytes(plain, xk));
    }
    // ① 实现方式：非 V2 一律全文件 XOR 后直接返回，不写任何头部。
    // ② 关键程序解析：与 decrypt_dat 的 V1 分支严格互逆（V1 容器没有头、没有长度字段）；
    //    这里不校验 plain 是否真是图片，调用方需自己保证输入是解密出的明文。
    // ③ 功能：复现 V1 容器。

    let Some(key16) = key16 else {
        bail!("dat v2 encrypt requires a 16+ char aesKey");
    };
    // ① 实现方式：let-else 二次确认密钥存在（显式传 meta.version=2 但没给密钥时会走到这里）。
    // ② 关键程序解析：上面的推断分支已保证"无密钥→version 1"，所以这条 bail 只在
    //    调用方**显式**要求 V2 却缺密钥时触发；错误串不回显密钥。
    // ③ 功能：守住 V2 加密的密钥前提。
    let aes_size = meta
        .and_then(|m| m.aes_size)
        .map(|v| v as usize)
        .unwrap_or(1023.min(plain.len()));
    if aes_size > plain.len() {
        bail!("aes_size {aes_size} exceeds plaintext length {}", plain.len());
    }
    // ① 实现方式：aes_size 取 meta 值，缺省用 1023 并夹到明文长度；随后校验不超过明文总长。
    // ② 关键程序解析：1023 是刻意取的**非 16 倍数**（与 TS 参考实现一致），这样 aligned=1024，
    //    能覆盖图片头 + 部分像素；`.min(plain.len())` 保证小文件不会越界。
    //    显式传入的 aes_size 若大于明文长度则 bail，避免 `plain[..aes_size]` panic。
    // ③ 功能：确定 AES 段覆盖的明文字节数。
    let xor_size = meta
        .and_then(|m| m.xor_size)
        .map(|v| v as usize)
        .unwrap_or(0)
        .min(plain.len() - aes_size);
    let flag = meta.and_then(|m| m.flag).unwrap_or(0) as u8;
    // ① 实现方式：xor_size 取 meta 值（缺省 0），再夹到"明文长度 - aes_size"以内；flag 缺省 0 并截成 u8。
    // ② 关键程序解析：上一段已保证 aes_size <= plain.len()，所以这里的减法不会下溢；
    //    夹紧而非报错是为了让"meta 与当前明文长度不匹配"时仍能产出合法容器（尾段变短）。
    //    flag 的 as u8 会静默截断 >255 的值，但解密侧读出的 flag 本就来自单字节，正常无损。
    // ③ 功能：确定 XOR 尾段长度与容器 flag。

    let aligned = aes_size + (16 - aes_size % 16);
    let enc = ecb_encrypt(&key16, &add_pkcs7(&plain[..aes_size]));
    debug_assert_eq!(enc.len(), aligned);
    // ① 实现方式：用与解密侧完全相同的公式算 aligned；对明文前 aes_size 字节补 PKCS7 后 ECB 加密。
    // ② 关键程序解析：debug_assert 只在 debug 构建生效，用来钉住"补填充后长度 == aligned"这条不变量；
    //    release 下即使公式被改坏也不会 panic，而是产出与解密侧不一致的容器（所以两侧公式必须同步维护）。
    // ③ 功能：生成 V2 的 AES 段密文。

    let raw = &plain[aes_size..plain.len() - xor_size];
    let xor_tail = xor_bytes(&plain[plain.len() - xor_size..], xk);
    // ① 实现方式：切出中段 raw（原样不加密）与尾段（XOR 加密）。
    // ② 关键程序解析：xor_size=0 时 `plain[len..]` 是合法的空切片、xor_tail 为空，raw 覆盖剩余全部；
    //    两个切片的边界都由前面的夹紧保证不越界、不重叠。
    // ③ 功能：准备 V2 的中段与尾段。

    let mut out = Vec::with_capacity(V2_HEADER_LEN + aligned + raw.len() + xor_tail.len());
    out.extend_from_slice(&SIG_V2);
    out.extend_from_slice(&(aes_size as u32).to_le_bytes());
    out.extend_from_slice(&(xor_size as u32).to_le_bytes());
    out.push(flag);
    // ① 实现方式：按 15 字节头布局依次写入 magic(6) + aesSize:i32le(4) + xorSize:i32le(4) + flag(1)。
    // ② 关键程序解析：写入顺序与偏移必须与 decrypt_dat 的 header[6..10]/[10..14]/[14] 严格对应；
    //    to_le_bytes 保证小端（微信容器约定），usize→u32 的 as 转换在 >4GB 时会截断（实际不触发）。
    // ③ 功能：生成 V2 容器头。
    out.extend_from_slice(&enc);
    out.extend_from_slice(raw);
    out.extend_from_slice(&xor_tail);
    Ok(out)
    // ① 实现方式：头之后按「AES 密文 + raw + XOR 尾段」拼接，返回完整容器字节。
    // ② 关键程序解析：三段顺序与 decrypt_dat 的拼接顺序一致，且总长 = 15 + aligned + raw + xor，
    //    解密侧据此反推 raw_len，因此任何一段长度写错都会导致 roundtrip 失败。
    //    本函数只服务编辑/重加密场景，只读导出链路不调用，也不写回微信数据目录。
    // ③ 功能：产出与解密侧字节级互逆的 V2 .dat 容器。
}

pub fn decrypt_dat_file(path: &str, xor_key: u32, aes_key: Option<&str>) -> Result<DecryptedDat> {
    let data = std::fs::read(path).map_err(|e| anyhow!("read {path}: {e}"))?;
    // ① 实现方式：一次性 std::fs::read 整个文件进内存，错误包成带路径的 anyhow。
    // ② 关键程序解析：纯**只读**打开，不写、不改 mtime 以外的任何属性（读文件会更新 atime）；
    //    没有文件大小上限，超大视频会整块进内存；错误串含调用方传入的路径，不含密钥。
    // ③ 功能：把磁盘上的 .dat 读成字节缓冲。
    decrypt_dat(&data, xor_key, aes_key)
    // ① 实现方式：直接委托内存版 decrypt_dat，参数原样透传。
    // ② 关键程序解析：所有版本判定/边界校验/格式识别都在 decrypt_dat 里，本函数只负责 I/O；
    //    返回的 DecryptedDat.data 是明文副本，调用方若落盘必须写到自己的输出目录，绝不写回原 .dat 路径。
    // ③ 功能：给上层提供"按路径解密"的便捷入口。
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_jpeg(len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        v[0] = 0xff;
        v[1] = 0xd8;
        v[2] = 0xff;
        // ① 实现方式：先分配 len 个 0，再把前 3 字节写成 JPEG 的 FF D8 FF 魔数。
        // ② 关键程序解析：detect_ext 只验前 3 字节，所以这 3 字节就足以让夹具被认成 "jpg"；
        //    len 必须 >= 12 才能过 detect_ext 的长度下界（各测试用的 100~5000 都满足）。
        // ③ 功能：造一个"格式上可识别"的最小明文图片。
        for i in 3..len {
            v[i] = (i % 251) as u8;
        }
        // ① 实现方式：其余字节填 (i % 251)，生成非全零、非周期 16 的伪随机内容。
        // ② 关键程序解析：刻意用质数 251 取模，避免内容出现 16 字节周期——否则 ECB 加密后
        //    大量重复块可能掩盖分段错误；同时保证尾部字节不会恰好等于 PKCS7 填充值而干扰断言。
        // ③ 功能：让 roundtrip 的字节比对真正有意义。
        v
        // ① 实现方式：返回构造好的夹具（纯内存，不碰磁盘、不碰微信数据目录）。
        // ② 关键程序解析：调用方拿到的就是"明文真值"，后面所有 assert_eq!(dec.data, plain) 都以它为基准。
        // ③ 功能：为加解密测试提供可重复的输入。
    }

    #[test]
    fn v2_roundtrip() {
        let plain = fake_jpeg(5000);
        let aes_key = "0123456789abcdef0123456789abcdef";
        let enc = encrypt_dat(&plain, 0x5a, Some(aes_key), None).unwrap();
        assert_eq!(&enc[..6], &SIG_V2);
        // ① 实现方式：用合成夹具密钥（32 字符，规范化时只取前 16 字节）+ 不传 meta 走缺省路径加密，
        //    断言输出前 6 字节是 V2 magic。
        // ② 关键程序解析：meta=None 时 version 由"有 aesKey → 2"推断、aes_size 缺省 1023、xor_size 缺省 0；
        //    密钥字面量是测试夹具，与任何真实账号密钥无关。
        // ③ 功能：验证 V2 容器头与缺省参数推断。
        let dec = decrypt_dat(&enc, 0x5a, Some(aes_key)).unwrap();
        assert_eq!(dec.data, plain);
        assert_eq!(dec.ext, "jpg");
        assert!(!dec.is_wxgf);
        assert_eq!(dec.version, 2);
        assert_eq!(dec.meta.aes_size, Some(1023));
        // ① 实现方式：用同一 XOR 密钥与 AES 密钥解密，逐字节比对明文，并校验 ext/is_wxgf/version/meta。
        // ② 关键程序解析：`dec.data == plain` 是最强的一条——它同时验证了 aligned 公式、PKCS7 补/去填充、
        //    三段拼接顺序全部正确；aes_size 断言钉住 1023 这个缺省值（改动缺省会让本条失败）。
        // ③ 功能：锁住 V2 加解密的字节级互逆性。
    }

    #[test]
    fn v2_roundtrip_with_meta() {
        let plain = fake_jpeg(3000);
        let aes_key = "abcdef9876543210";
        let meta = DatMeta {
            version: Some(2),
            aes_size: Some(512),
            xor_size: Some(256),
            raw_size: None,
            flag: Some(7),
        };
        // ① 实现方式：显式构造 meta——aes_size=512（16 的倍数）、xor_size=256、flag=7、raw_size 故意留 None。
        // ② 关键程序解析：aes_size 取 16 倍数专门覆盖"aligned 要额外加一整块填充"的边界；
        //    raw_size=None 验证重加密**不依赖**入参 raw_size（它由 plain.len()-aes_size-xor_size 推回）。
        // ③ 功能：准备一组非缺省的分段参数。
        let enc = encrypt_dat(&plain, 0xa5, Some(aes_key), Some(&meta)).unwrap();
        assert_eq!(enc[14], 7);
        // ① 实现方式：按 meta 加密后直接检查偏移 14 的 flag 字节。
        // ② 关键程序解析：enc[14] 正是 15 字节头的最后一字节，这条断言钉死了 flag 的字节位置，
        //    头布局一旦被改动立刻失败。
        // ③ 功能：验证 flag 写入偏移。
        let dec = decrypt_dat(&enc, 0xa5, Some(aes_key)).unwrap();
        assert_eq!(dec.data, plain);
        assert_eq!(dec.meta.xor_size, Some(256));
        assert_eq!(dec.meta.flag, Some(7));
        // ① 实现方式：解密后比对明文，并断言 xor_size 与 flag 被原样读回。
        // ② 关键程序解析：三段都非空（AES 512 + raw 2232 + XOR 256）时仍能字节级还原，
        //    说明中段/尾段分界计算正确；meta 回读一致保证"解密→重加密"可无损往返。
        // ③ 功能：锁住带完整 meta 的 V2 roundtrip。
    }

    #[test]
    fn v1_roundtrip() {
        let plain = fake_jpeg(800);
        let enc: Vec<u8> = plain.iter().map(|b| b ^ 0x3c).collect();
        let dec = decrypt_dat(&enc, 0x3c, None).unwrap();
        assert_eq!(dec.data, plain);
        assert_eq!(dec.version, 1);
        // ① 实现方式：手工全文件 XOR 造出 V1 密文（不走 encrypt_dat），再用 aes_key=None 解密。
        // ② 关键程序解析：密文前 3 字节被 XOR 后不再是 JPEG 魔数，所以不会命中"明文直通"分支；
        //    容器也没有 V1/V2 magic，dat_version 返回 0，最终由 v1_version 归一成 1——这条断言钉住该归一逻辑。
        // ③ 功能：验证 V1 解密路径与 version=0→1 的归一。
        let re = encrypt_dat(&plain, 0x3c, None, Some(&DatMeta { version: Some(1), ..Default::default() })).unwrap();
        assert_eq!(re, enc);
        // ① 实现方式：显式传 version=1 的 meta 重加密，断言输出与手工 XOR 的密文**完全相等**。
        // ② 关键程序解析：aes_key=None + version=1 保证走"非 V2 → 全文件 XOR、不写头"分支；
        //    若误加了 15 字节头或误判成 V2，长度与字节都会不同，断言立即失败。
        // ③ 功能：锁住 V1 重加密的字节级互逆性。
    }

    #[test]
    fn plaintext_passthrough() {
        let plain = fake_jpeg(100);
        let dec = decrypt_dat(&plain, 0x11, None).unwrap();
        assert_eq!(dec.version, 0);
        assert_eq!(dec.ext, "jpg");
        // ① 实现方式：直接把**未加密**的夹具喂给 decrypt_dat，断言 version=0 且 ext="jpg"。
        // ② 关键程序解析：传了 xor_key=0x11 却必须原样返回——证明明文直通分支在 XOR 之前生效，
        //    否则内容会被异或破坏；version=0 是上层区分"没加密"与"V1 解密结果"的唯一依据。
        // ③ 功能：锁住明文直通行为，防止对未加密 .dat 做多余 XOR。
    }

    #[test]
    fn wxgf_detection() {
        let mut data = vec![0u8; 64];
        data[..4].copy_from_slice(b"wxgf");
        assert_eq!(detect_ext(&data), Some("wxgf"));
        // ① 实现方式：造 64 字节缓冲、前 4 字节写 ASCII "wxgf"，断言 detect_ext 认得它。
        // ② 关键程序解析：64 字节满足 >=12 的下界；其余字节全 0 不影响识别（只看前 4 字节）。
        // ③ 功能：验证 WXGF 魔数识别。
        // 全 XOR 的 V1 容器
        let enc: Vec<u8> = data.iter().map(|b| b ^ 0x77).collect();
        let dec = decrypt_dat(&enc, 0x77, None).unwrap();
        assert!(dec.is_wxgf);
        assert_eq!(dec.ext, "wxgf");
        // ① 实现方式：把 WXGF 明文全文件 XOR 成 V1 容器，解密后断言 is_wxgf 与 ext 同时正确。
        // ② 关键程序解析：XOR 密钥 0x77 恰好等于 'w'，所以密文首字节变成 0x00——这说明识别发生在
        //    **解密之后**；is_wxgf 与 ext 两个字段必须一致（上层 TS 两个都读）。
        // ③ 功能：验证 WXGF 经 V1 加密后仍能被正确识别并标记。
    }

    #[test]
    fn v2_without_key_fails() {
        let enc = encrypt_dat(&fake_jpeg(2000), 0x22, Some("0123456789abcdef"), None).unwrap();
        assert!(decrypt_dat(&enc, 0x22, None).is_err());
        assert!(decrypt_dat(&enc, 0x22, Some("short")).is_err());
        // ① 实现方式：先用夹具密钥造出 V2 容器，再分别以"无密钥"和"过短密钥"解密，两条都必须 is_err。
        // ② 关键程序解析：覆盖 normalize_aes_key 的 None 与 len<16 两个拒绝分支，
        //    确认 V2 缺密钥时**不会降级成全文件 XOR** 产出乱码（与 TS"不产生候选"的行为对齐）。
        // ③ 功能：锁住 V2 的密钥强制要求。
    }

    #[test]
    fn unrecognized_output_fails() {
        let noise: Vec<u8> = (0..512u32).map(|i| (i * 7 % 256) as u8).collect();
        assert!(decrypt_dat(&noise, 0x99, None).is_err());
        // ① 实现方式：造 512 字节线性同余噪声，XOR 解密后断言整体 is_err。
        // ② 关键程序解析：噪声既不含容器 magic 也不含任何其他魔数，XOR 后仍然认不出格式，
        //    必须走 detect_ext → None → anyhow 错误这条路，而不是返回一个 ext 为空的"成功"结果。
        // ③ 功能：锁住"解密后格式不认识就失败"的兜底判据（上层据此回退 JS 解密路径）。
    }
}
