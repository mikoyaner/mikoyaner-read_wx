"""消息内容渲染：原始 local_type/content → 导出列⑥「消息内容」。

类型映射与占位符口径移植自 v1 reply-exporter 的 render_message（本机 4.1.15 实测 20 种取值）：
- 微信 4.x 的 local_type 是复合值：高 32 位 = appmsg 子类型，低 32 位 = 基础类型；
- 语音（base 34）优先走 ASR 转文字（用户拍板），失败/未配置回退 [语音] 占位符；
- 其余非文本按占位符；引用消息抽「被引用原话+本条回复」，链接/文件/合并转发抽标题。

XML 解析全部是朴素字符串处理（继承 v1）：对微信的半合法 XML 更宽容，且零第三方依赖。
"""
# 实现：模块 docstring 声明职责、复合类型拆解与占位符口径来源。
# 功能：outfile/cli 渲染消息内容列的唯一出口。

from __future__ import annotations

# 实现：零依赖（纯字符串处理）。
# 功能：本模块全部依赖。

# 基础类型 → 固定占位符（v1 render_message 同款文案）。
_PLACEHOLDER = {
    3: "[图片]",
    43: "[视频]",
    47: "[动画表情]",
    48: "[位置]",
    50: "[语音/视频通话]",
    10000: "[系统消息]",
}
# 实现：六种固定占位的基础类型。
# 解析：34（语音）不在表里——它有 ASR 分支，单独处理；17/49 有标题/引用抽取分支，也不在表里。
# 功能：占位符文案的唯一真值表。

_QUOTE_CAP = 200       # 被引用原话截断长度（v1 同款）
_REPLY_CAP = 300       # 本条回复截断长度（v1 同款：回复是重点，给得更宽）
# 实现：引用/回复两段的字符上限常量。
# 解析：chars 截断（不是字节），防切断 UTF-8；上限给出境文本确定体积上界（v1 口径）。
# 功能：extract_quote 的长度收敛参数。


def split_local_type(local_type: int) -> tuple[int, int]:
    """复合 local_type 拆解：低 32 位 = 基础类型，高 32 位 = appmsg 子类型（v1 同款位运算）。"""
    raw = local_type & 0xFFFFFFFFFFFFFFFF  # 负数（i64 越界成负）安全转无符号
    return (raw & 0xFFFFFFFF, raw >> 32)
    # 实现：先按 u64 收拢（Rust 侧 i64 传来的大数在 JSON 里可能是负数），再位与取低 32 位、右移取子类型。
    # 解析：JSON 里 local_type 是 i64；49+57 复合值 = 244813135921 为正，但防御性处理负数形态。
    # 功能：所有类型分派的统一入口。


def is_hex_blob(s: str) -> bool:
    """是否「BLOB 被 hex 化」的内容：长度 >=48、偶数、全 hex 字符（v1 同口径）。"""
    return len(s) >= 48 and len(s) % 2 == 0 and all(c in "0123456789abcdefABCDEF" for c in s)
    # 实现：三条判据与 v1 完全一致。
    # 解析：48 是经验阈值（24 字节以上二进制才像卡片/富消息）；导出器已尽力 zstd 解压，
    #       还剩 hex 形态的 type 1 内容就是解不开的二进制卡片。
    # 功能：type 1 分支的降级判据。


def xml_unescape(s: str) -> str:
    """还原五种 XML 实体；&amp 必须最后还原（先还原会让变出的 & 被误处理，v1 同款顺序）。"""
    return (s.replace("&lt;", "<").replace("&gt;", ">")
             .replace("&quot;", '"').replace("&apos;", "'")
             .replace("&amp;", "&"))
    # 实现：固定顺序 replace。
    # 功能：xml_tag_inner 的最后一步。


def xml_tag_inner(xml: str, tag: str) -> str | None:
    """取 `<tag …>…</tag>` 的内文（支持带属性、自闭合返回 None、CDATA 剥壳；v1 同款朴素实现）。"""
    open_tag = f"<{tag}"
    start = xml.find(open_tag)
    if start < 0:
        return None
    after = start + len(open_tag)
    gt = xml.find(">", after)
    if gt < 0:
        return None
    if xml[after:gt].rstrip().endswith("/"):
        return None  # 自闭合标签没有内文
    # 实现：前缀定位 + 属性区跳过 + 自闭合识别（v1 同款；前缀匹配的已知坑继承——
    #       <content 也会命中 <contentxyz，微信真实数据里未触发）。
    # 功能：定位内文起点。
    body_start = gt + 1
    close = f"</{tag}>"
    end = xml.find(close, body_start)
    if end < 0:
        return None
    body = xml[body_start:end]
    if body.startswith("<![CDATA[") and body.endswith("]]>"):
        body = body[9:-3]
    # 实现：第一个同名闭合标签之间的内容；CDATA 包裹则剥壳（v1 同款）。
    # 功能：抽取内文并还原实体。
    return xml_unescape(body.strip())
    # 功能：交付还原后的内文。


def strip_xml_tags(s: str) -> str:
    """剥掉 XML 标签留纯文本：单遍状态机 + 连续空白压成单空格（v1 同款）。"""
    out = []
    in_tag = False
    for c in s:
        if c == "<":
            in_tag = True
        elif c == ">":
            in_tag = False
        elif not in_tag:
            out.append(c)
    return " ".join("".join(out).split())
    # 实现：与 v1 完全一致的状态机。
    # 解析：已知坑（继承 v1）：字面量尖括号区间会被吃掉、不处理 CDATA——引用场景可接受。
    # 功能：被引用内容（常是转义的内嵌 XML）转纯文本。


def extract_quote(content: str) -> str | None:
    """引用消息（49+57）：抽出「被引用的谁说了什么」+「本条回复正文」（v1 同款文案）。"""
    who = xml_tag_inner(content, "displayname") or ""
    quoted = xml_tag_inner(content, "content")
    if quoted is not None:
        quoted = strip_xml_tags(xml_unescape(quoted)).strip() or None
    reply = xml_tag_inner(content, "title")
    if reply is not None:
        reply = reply.strip() or None
    # 实现：displayname 取不到给空串；refermsg/content 走 unescape→剥标签→trim（空归 None）；
    #       appmsg/title 是本条回复正文，人手敲的纯文本，刻意不剥标签（v1 口径）。
    # 解析：两段都空才放弃（由调用方回退占位）——只丢被引用内容不丢回复（v1 修复过的坑）。
    # 功能：取出引用消息的两个语义段。
    if quoted is None and reply is None:
        return None

    def cap(s: str, n: int) -> str:
        return s[:n]
    # 实现：chars 截断（Python 字符串切片天然按字符，无 UTF-8 截断问题）。
    parts: list[str] = []
    if quoted:
        parts.append(f"[引用 {who.strip()}：{cap(quoted, _QUOTE_CAP)}]" if who.strip()
                     else f"[引用：{cap(quoted, _QUOTE_CAP)}]")
    if reply:
        parts.append(f"[本条回复：{cap(reply, _REPLY_CAP)}]")
    return " ".join(parts)
    # 实现：文案与 v1 逐字一致（[引用 谁：…]/[引用：…] + [本条回复：…]）。
    # 功能：渲染引用消息的可读形态。


def extract_title(content: str) -> str | None:
    """从 XML/文本里朴素抽 title/des/来源名（v1 同款四锚点）。"""
    for key in ['title="', "<title>", 'des="', 'source-displayname="']:
        start = content.find(key)
        if start < 0:
            continue
        rest = content[start + len(key):]
        # v1 语义：优先取到下一个双引号为止；没有双引号才取尖括号；都没有取到串尾。
        # （不是取两者中更近者——引号存在时即使尖括号更近也以引号为准。）
        end = rest.find('"')
        if end < 0:
            end = rest.find("<")
        stop = end if end >= 0 else len(rest)
        val = rest[:stop].strip()
        if val:
            return val
    return None
    # 实现：按优先级试 title 属性、title 元素、des 属性、source-displayname 属性，
    #       值取到下一个双引号（优先）或尖括号为止（都没有取到串尾），trim 后非空即返回。
    # 解析：已知坑（继承 v1）：不做实体还原、CDATA 形态截出空串、字面量尖括号提前截断——
    #       四锚点按序兜底后的实测命中率足够（v1 本机验证过）。
    # 功能：给合并转发/链接/文件消息尽量抽一个可读标题。


def render_content(row: dict, transcribe=None) -> str:
    """单条消息 → 导出列⑥「消息内容」。

    row：导出器 --messages 的消息行（local_type/content/voice）。
    transcribe：可选回调 (voice_file_name) -> str|None，由 voice.py 提供；
    给了且回调产出非空文本时语音消息用转写文本，否则回退 [语音]。
    """
    local_type = int(row.get("local_type") or 0)
    base, sub = split_local_type(local_type)
    content = row.get("content") or ""
    # 实现：行字段宽松取值（缺键/None → 0/空串），与导出器 JSON 形态对齐。
    # 功能：渲染入口。
    if base == 1:
        # 文本消息：明文直接用；仍是 hex 形态 = 解不开的二进制卡片（zstd 解压已尽力）。
        return "[卡片/链接消息]" if is_hex_blob(content) else content
    if base == 34:
        # 语音：ASR 转文字（用户拍板）；失败/未配置/转写为空 → [语音] 占位符，绝不静默丢消息。
        voice = row.get("voice") or {}
        if transcribe is not None and voice.get("status") == "extracted" and voice.get("file"):
            text = transcribe(voice["file"])
            if text:
                return text
        return "[语音]"
    if base == 49:
        # 应用消息家族：57=引用、19=合并转发、其余（链接/小程序/文件等）抽标题，
        # 标题抽不到才回退通用占位（v1 口径；appmsg 的 url/附件原始字段一律不进导出）。
        if sub == 57:
            return extract_quote(content) or "[引用消息]"
        if sub == 19:
            return extract_title(content) or "[合并转发消息]"
        return extract_title(content) or "[链接/小程序/文件分享]"
    if base == 17:
        return "[合并转发消息]"          # 裸 17（旧口径，v1 实测出现 0 次但保留分派）
    if base in _PLACEHOLDER:
        return _PLACEHOLDER[base]
    # 未识别类型：占位符不带原始编号（v1 口径——编号会被当敏感数字，原始值留在 local_type 字段里）。
    return "[非文本消息]"
    # 实现：类型分派顺序 = 1(文本) → 34(语音/ASR) → 49 家族(引用/转发/标题) → 17 → 固定占位 → 兜底。
    # 解析：与 v1 render_message 的 match 分支一一对应；文案逐字一致（本机已验证的口径）。
    # 功能：导出列⑥的唯一渲染实现。
