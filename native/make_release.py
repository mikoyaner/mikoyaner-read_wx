"""发版辅助：编译后的 exe 隐私补丁 + 绿色包打包。

用法：python native/make_release.py
做三件事：① 对 bin/wxmsg-exporter.exe 做同长度隐私补丁（编译机用户名/项目绝对路径，
Rust panic 定位字符串会嵌入这些）；② 核对隐私特征清零；③ 打绿色包 zip 到 dist/
（源码 + exe + 入口，不含 .env/cache/output/密钥库）。zip 供上传 GitHub Releases。
"""
import glob
import os
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
EXE = ROOT / "bin" / "wxmsg-exporter.exe"

# 同长度替换表（搜索串与替换串长度必须一致——PE 内偏移不能移动，否则段错误）
PATCHES = [
    (b"Mikoyan", b"redact1"),
    (b"D:" + b"\\" + b"Export_from_WeChat" + b"\\" + b"read_wechat",
     b"D:" + b"\\" + b"x" * 30),
]


def patch_exe() -> None:
    """对 exe 做同长度隐私覆写并断言大小不变（PE 内偏移不可移动）。"""
    data = EXE.read_bytes()
    orig = len(data)
    for search, repl in PATCHES:
        assert len(search) == len(repl), "补丁串必须等长"
        data = data.replace(search, repl)
    assert len(data) == orig, "补丁后大小变化——PE 会段错误，禁止写出"
    EXE.write_bytes(data)
    print(f"[patch] {EXE.name} 隐私补丁完成（大小不变 {orig}）")
    # 功能：抹掉 panic 定位字符串里嵌入的编译机用户名与项目绝对路径。


def verify() -> None:
    """核对隐私特征清零。"""
    data = EXE.read_bytes()
    for _, search in PATCHES:
        assert search not in data, f"残留: {search!r}"
    for pat in (b"E:/xwechat_files", b"8vmf", b"yx99"):
        assert pat not in data, f"残留: {pat!r}"
    print("[verify] 隐私特征清零 ✓")
    # 功能：补丁后的强制核对。


def pack() -> None:
    """打绿色包 zip 到 dist/（源码 + exe + 入口；排除隐私与构建产物）。"""
    dist = ROOT / "dist"
    dist.mkdir(exist_ok=True)
    files = sorted(glob.glob(str(ROOT / "*" )) ) 
    include = []
    for p in files:
        name = os.path.basename(p)
        if name in ("start_panel.bat", "config.example", "README.md", "AGENTS.md", "LICENSE"):
            include.append(Path(p))
    for sub in ("wxmsg", "tests"):
        base = ROOT / sub
        for dirpath, _dirs, fns in os.walk(base):
            if "__pycache__" in dirpath:
                continue
            for fn in fns:
                include.append(Path(dirpath) / fn)
    for sub in ("shared/weflow-core/src", "wcdb/src", "wxmsg-exporter/src"):
        base = ROOT / "native" / sub
        for dirpath, _dirs, fns in os.walk(base):
            for fn in fns:
                if fn.endswith((".rs", ".toml")):
                    include.append(Path(dirpath) / fn)
    for extra in ("native/Cargo.toml", "native/Cargo.lock", ".cargo/config.toml",
                  "conftest.py", "keyscan_readonly.py", "bin/wxmsg-exporter.exe"):
        p = ROOT / extra
        if p.is_file():
            include.append(p)
    zpath = dist / "read_wechat_portable.zip"
    with zipfile.ZipFile(zpath, "w", zipfile.ZIP_DEFLATED) as z:
        for p in include:
            z.write(p, str(Path("read_wechat") / p.relative_to(ROOT)))
    print(f"[pack] {zpath}（{zpath.stat().st_size // 1024} KB，{len(include)} 个文件）")
    # 实现：白名单打包（宁可少带不能多带——聊天明文/密钥/本机配置绝不入包）。
    # 功能：生成可直接上传 GitHub Releases 的绿色包。


if __name__ == "__main__":
    patch_exe()
    verify()
    pack()
