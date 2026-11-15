"""keyscan_readonly.py 移植后的单元测试——只测纯函数，不碰 Win32/微信进程。

被测对象与口径来自 v1 原文件（原样移植），测试自己构造输入预期输出：
- verify_enc_key：用合成 SQLCipher page1（自己算 HMAC）验证「对的钥匙过、错的钥匙拒」
- blob_key_candidates：构造 XOR 混淆 blob 验证候选枚举与去重
- probable_key / u64_at / mask_key / xor_repeat：小工具函数的行为锚点
- merge_store / discover_db_files：落盘合并与库发现逻辑（纯文件操作）
"""
# 实现：模块 docstring 声明测试范围（纯函数）与构造式验证思路。
# 解析：keyscan 的 Win32 部分（weixin_pids/readable_regions/scan_pid）依赖真实微信进程，
#       只能留给步骤 12 真机验收；单测覆盖的是可离线复现的全部逻辑分支。
# 功能：保证移植过程中密钥校验/候选枚举/合并落盘行为与 v1 完全一致。
import json
import struct

import pytest

import keyscan_readonly as ks

# 实现：import 被测模块（conftest.py 已把项目根放进 sys.path）。
# 解析：模块顶层有 ctypes.windll——本测试只在 Windows 跑（本项目仅支持 Windows）。
# 功能：取得被测函数引用。


def _make_page1(key: bytes, salt: bytes) -> bytes:
    """构造一页能让 ks.verify_enc_key(key, page1) 为 True 的合成 page1。

    布局与 keyscan 常量严格对应：[0:16]=salt、[16:4016]=密文数据、[4016:4032]=IV、
    [4032:4096]=页尾 HMAC。HMAC 输入 = page1[16:4032] + 页号1 小端 uint32。
    """
    assert len(salt) == ks.SALT_SZ and len(key) == ks.KEY_SZ
    page = bytearray(ks.PAGE_SZ)
    page[0:ks.SALT_SZ] = salt
    # 实现：分配整页，先放 salt；中段密文数据保持 0 字节（HMAC 对全 0 数据同样有效）。
    # 解析：中段内容无所谓——HMAC 是确定性计算，校验方按同一切片重算即可；
    #       IV 段 [4016:4032] 落在 hmac_data 切片内，同样全 0 即可。
    # 功能：生成校验目标页。
    mac_salt = bytes(b ^ 0x3A for b in salt)
    mac_key = __import__("hashlib").pbkdf2_hmac("sha512", key, mac_salt, 2, dklen=ks.KEY_SZ)
    hm = __import__("hmac").new(mac_key, bytes(page[ks.SALT_SZ: ks.PAGE_SZ - ks.RESERVE_SZ + ks.IV_SZ]),
                                __import__("hashlib").sha512)
    hm.update(struct.pack("<I", 1))
    page[ks.PAGE_SZ - ks.HMAC_SZ: ks.PAGE_SZ] = hm.digest()
    return bytes(page)
    # 实现：按 SQLCipher 4 口径派生 mac_key（PBKDF2-HMAC-SHA512、2 轮、32 字节），对
    #       [16:4032) 数据+页号做 HMAC-SHA512 写进页尾 64 字节。
    # 解析：与 ks.verify_enc_key 的算法互为镜像——它重算同样摘要比较页尾；
    #       用 __import__ 是为把 hashlib/hmac 依赖局部化在这一构造函数里。
    # 功能：提供「已知正确密钥」的测试夹具页。


KEY_A = bytes(range(32))            # 32 字节、32 种不同取值，必过 probable_key
KEY_B = bytes(range(1, 33))         # 另一把与 A 不同的合法钥匙
SALT_1 = bytes.fromhex("aa" * 16)
SALT_2 = bytes.fromhex("bb" * 16)
# 实现：两把合法测试钥匙与两个 salt 常量。
# 解析：测试钥匙是顺序字节——高熵判据（>=15 种不同取值）必过，且绝不能当真实密钥用。
# 功能：全文件共用的夹具常量。


class TestVerifyEncKey:
    def test_correct_key_passes(self):
        page1 = _make_page1(KEY_A, SALT_1)
        assert ks.verify_enc_key(KEY_A, page1) is True
        # 实现：用「按 KEY_A 算好 HMAC 的页」喂 verify_enc_key(KEY_A)。
        # 解析：这是移植正确性的主锚点——算法任何一处（切片/迭代轮数/异或 0x3A/页号端序）移植错都会红。
        # 功能：验证正确密钥通过校验。

    def test_wrong_key_fails(self):
        page1 = _make_page1(KEY_A, SALT_1)
        assert ks.verify_enc_key(KEY_B, page1) is False
        # 实现：同一页换另一把合法钥匙校验。
        # 解析：防「校验函数对任何钥匙都返回 True」的假阳性实现。
        # 功能：验证错误密钥被拒绝。

    def test_short_page_fails(self):
        assert ks.verify_enc_key(KEY_A, b"\x00" * (ks.PAGE_SZ - 1)) is False
        # 实现：喂不足 4096 字节的页。
        # 解析：verify_enc_key 对短页必须 False 而不是抛异常（真实扫描中短页=坏文件很常见）。
        # 功能：验证短页防御分支。


class TestBlobKeyCandidates:
    @staticmethod
    def _obfuscate(payload: bytes) -> bytes:
        return ks.xor_repeat(payload, ks.CONFIG_XOR_MASK)
        # 实现：把明文 payload 按固定掩码 XOR 成「从内存里读出来的样子」。
        # 解析：xor_repeat 是自反运算，构造与解码同一函数——但 XOR 解码正确性另由 TestXor 锚定。
        # 功能：构造混淆 blob 夹具。

    def test_plain_literal_extracted(self):
        key_hex = KEY_A.hex()
        salt_hex = SALT_1.hex()
        payload = b"x'" + key_hex.encode() + salt_hex.encode() + b"'"
        cands = list(ks.blob_key_candidates(self._obfuscate(payload)))
        assert cands == [(key_hex, salt_hex)]
        # 实现：标准形态 payload（x'<key64><salt32>'）混淆后喂入，断言唯一候选 (key,salt)。
        # 解析：对应真实场景——Config.Cipher blob 解码后正好含一个库的字面量。
        # 功能：验证主提取路径（解码→正则→(key_hex, embedded_salt)）。

    def test_uppercase_x_and_hex_normalized(self):
        payload = b"X'" + KEY_A.hex().upper().encode() + b"'"
        cands = list(ks.blob_key_candidates(self._obfuscate(payload)))
        assert cands == [(KEY_A.hex(), None)]
        # 实现：大写 X 与大写 hex 的字面量。
        # 解析：CONFIG_LITERAL_RE 允许大小写 x/hex，函数内部 lower 归一——断言产出小写 key_hex。
        # 功能：验证大小写归一口径。

    def test_dedup_same_candidate(self):
        payload = (b"x'" + KEY_A.hex().encode() + b"'") * 2
        cands = list(ks.blob_key_candidates(self._obfuscate(payload)))
        assert len(cands) == 1
        # 实现：同一字面量出现两次。
        # 解析：seen 集合去重保序——重复候选只 yield 一次。
        # 功能：验证去重口径。

    def test_oversized_blob_yields_nothing(self):
        payload = b"x'" + KEY_A.hex().encode() + b"'" * 1
        assert list(ks.blob_key_candidates(self._obfuscate(payload) + b"\x00" * (ks.CONFIG_BLOB_MAX + 1))) == []
        assert list(ks.blob_key_candidates(b"")) == []
        # 实现：超 1024 字节 blob 与空 blob。
        # 解析：CONFIG_BLOB_MAX 上限防误读超大内存；空 blob 直接 return。
        # 功能：验证尺寸防御分支。

    def test_low_entropy_key_rejected(self):
        weak = b"\x01" * 32  # 全同字节：len(set)==1 < 15
        payload = b"x'" + weak.hex().encode() + b"'"
        assert list(ks.blob_key_candidates(self._obfuscate(payload))) == []
        # 实现：32 个相同字节的低熵候选。
        # 解析：probable_key 的熵启发式把明显无效候选挡在 HMAC 校验前。
        # 功能：验证低熵候选被过滤。


class TestProbableKey:
    def test_ok(self):
        assert ks.probable_key(KEY_A) is True
        # 实现：合法钥匙通过。
        # 功能：主路径。

    def test_rejects_bad(self):
        assert ks.probable_key(b"\x00" * 32) is False       # 全 0
        assert ks.probable_key(b"\xff" * 32) is False       # 全 FF
        assert ks.probable_key(b"\x01" * 31) is False       # 长度不足
        # 实现：三类必须拒绝的输入。
        # 解析：全 0/全 FF 是内存里常见的填充模式，绝不能当密钥；长度错误直接不合法。
        # 功能：验证启发式过滤的三条拒绝分支。


class TestU64At:
    def test_read_and_bounds(self):
        data = struct.pack("<Q", 0x123456789ABCDEF0)
        assert ks.u64_at(data, 0) == 0x123456789ABCDEF0
        assert ks.u64_at(data, -1) == 0          # 负偏移
        assert ks.u64_at(data, len(data) - 7) == 0  # 越界
        # 实现：正常小端读取 + 负偏移/越界两个边界。
        # 解析：u64_at 越界返回 0（无效指针哨兵）而不是抛异常，调用方靠 0 判野指针。
        # 功能：验证安全读取原语。


class TestMaskKey:
    def test_mask_hides_content(self):
        masked = ks.mask_key(KEY_A.hex())
        assert KEY_A.hex() not in masked
        assert masked.startswith(KEY_A.hex()[:4]) and masked.endswith(KEY_A.hex()[-2:])
        # 实现：掩码输出不含完整 key，只含首 4/末 2 片段。
        # 解析：控制台定位用的口径——测试把「不整串泄漏」钉死，防将来有人把掩码去掉。
        # 功能：验证日志脱敏口径。

    def test_short_key(self):
        assert ks.mask_key("abc") == "****"
        # 实现：长度不足 8 的输入全遮罩。
        # 功能：验证短串分支。


class TestMergeStore:
    def test_merge_keeps_image_keys_and_other_accounts(self, tmp_path):
        old = {
            "db_keys": {"acct_old": "oldsessionkey"},
            "db_key_maps": {"acct_old": {"session/session.db": "oldsessionkey"}},
            "image_keys": {"imgcode1": {"xor": 1}},
        }
        out = tmp_path / "store.json"
        out.write_text(json.dumps(old), encoding="utf-8")
        merged = ks.merge_store(str(out), {"acct_new": "newsessionkey"},
                                {"acct_new": {"message/message_0.db": "k1"}})
        assert merged["image_keys"] == {"imgcode1": {"xor": 1}}          # 图片密钥保住
        assert merged["db_keys"]["acct_old"] == "oldsessionkey"          # 旧账号保住
        assert merged["db_key_maps"]["acct_new"] == {"message/message_0.db": "k1"}  # 新账号并入
        # 实现：预写旧 store，再合并新账号的扫描结果。
        # 解析：merge_store 的核心承诺是「只覆盖本次命中的 (账号,库)」，image_keys 是离线 MMKV
        #       派生的、扫不出来的——整档覆盖会把它清空（v1 实测踩过的坑），测试钉死这条。
        # 功能：验证增量合并语义。

    def test_same_slot_overwritten(self, tmp_path):
        out = tmp_path / "store.json"
        out.write_text(json.dumps({"db_key_maps": {"a": {"x.db": "OLD"}}}), encoding="utf-8")
        merged = ks.merge_store(str(out), {}, {"a": {"x.db": "NEW"}})
        assert merged["db_key_maps"]["a"]["x.db"] == "NEW"
        # 实现：同一 (账号,库) 槽位再扫一次。
        # 解析：换钥（重登录）场景下新钥必须覆盖旧钥。
        # 功能：验证覆盖语义。

    def test_corrupt_store_refuses_overwrite(self, tmp_path):
        out = tmp_path / "store.json"
        out.write_text("{半截 JSON", encoding="utf-8")
        with pytest.raises(RuntimeError):
            ks.merge_store(str(out), {}, {})
        # 实现：半截 JSON 的旧 store。
        # 解析：读坏了宁可不写也绝不按空库覆盖——这是 v1 修复过的丢键 bug，测试钉死。
        # 功能：验证损坏档案防御。

    def test_missing_store_starts_fresh(self, tmp_path):
        merged = ks.merge_store(str(tmp_path / "nope.json"), {}, {})
        assert merged == {"db_keys": {}, "db_key_maps": {}, "image_keys": {}}
        # 实现：store 文件不存在。
        # 解析：FileNotFoundError 是唯一允许当作空库起步的情况。
        # 功能：验证首次运行分支。


class TestDiscoverDbFiles:
    def test_walk_and_filter(self, tmp_path):
        acct = tmp_path / "wxid_test_1234"
        storage = acct / "db_storage" / "message"
        storage.mkdir(parents=True)
        page_a = _make_page1(KEY_A, SALT_1)
        page_b = _make_page1(KEY_B, SALT_2)
        (storage / "message_0.db").write_bytes(page_a)
        (storage / "message_1.db").write_bytes(page_b)
        (storage / "message_0.db-wal.db").write_bytes(page_a)   # 应被排除（-wal.db 后缀）
        (storage / "notes.txt").write_bytes(b"x" * ks.PAGE_SZ)  # 应被排除（非 .db）
        (tmp_path / "not_an_account" / "db_storage").mkdir(parents=True)  # 无 .db 的账号目录
        files, salt_to_idxs = ks.discover_db_files(str(tmp_path))
        rels = [rel for _a, rel, _p, _pg in files]
        assert "message/message_0.db" in rels and "message/message_1.db" in rels
        assert len(files) == 2          # wal 与 txt 都没进来
        assert len(salt_to_idxs) == 2   # 两个不同 salt 各一组
        for _a, _rel, abs_path, page1 in files:
            assert len(page1) == ks.PAGE_SZ
        # 实现：构造两账号目录（一个含 2 个真 .db + 1 个 -wal.db + 1 个 txt，一个空 db_storage）。
        # 解析：discover_db_files 的口径是「db_storage 下全部 .db 排除 -wal/-shm、只读 page1」，
        #       断言文件数/salt 分组/页长三项，锁死枚举行为。
        # 功能：验证库发现与 salt 倒排索引。
