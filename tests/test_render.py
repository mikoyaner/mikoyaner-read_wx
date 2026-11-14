"""wxmsg/render.py 的单元测试——类型分派、XML 抽取、占位符文案的口径锚点。"""
# 实现：模块 docstring 声明测试范围（纯函数）。
# 功能：锁定导出列⑥的渲染口径（与 v1 render_message 逐字一致）。
from wxmsg.render import (
    extract_quote, extract_title, is_hex_blob, render_content, split_local_type,
    strip_xml_tags, xml_tag_inner, xml_unescape,
)
# 实现：import 被测函数。
# 功能：取得被测对象。

_QUOTE_XML = (
    '<msg><appmsg appid="" sdkver="0"><title>本条回复正文</title><type>57</type>'
    '<refermsg><type>1</type><displayname>张三</displayname>'
    "<content>被引用的原话</content></refermsg></appmsg></msg>"
)
# 实现：引用消息的标准形态夹具（49+57：title=本条回复，refermsg 里 displayname+content）。
# 功能：extract_quote/render_content 共用夹具。


class TestSplitLocalType:
    def test_composite_and_plain(self):
        assert split_local_type(1) == (1, 0)
        assert split_local_type(49) == (49, 0)
        assert split_local_type((57 << 32) | 49) == (49, 57)
        assert split_local_type((19 << 32) | 49) == (49, 19)
        # 实现：复合值 = 高32位子类型 + 低32位基础类型（v1 同款位运算）。
        # 解析：244813135921 = (57<<32)|49 是本机实测的引用消息形态。
        # 功能：复合类型拆解锚点。


class TestHexBlob:
    def test_judgement(self):
        assert is_hex_blob("ab" * 24) is True      # 48 字符全 hex
        assert is_hex_blob("ab" * 23) is False     # 46 字符 < 48
        assert is_hex_blob("ab" * 24 + "z") is False  # 非 hex 字符
        # 实现：三条判据（长度>=48/偶数/全hex）。
        # 功能：type1 降级判据锚点。


class TestXmlHelpers:
    def test_tag_inner_variants(self):
        assert xml_tag_inner("<a><title>hi</title></a>", "title") == "hi"
        assert xml_tag_inner('<msg><title x="1">带属性</title></msg>', "title") == "带属性"
        assert xml_tag_inner("<t><![CDATA[原话]]></t>", "t") == "原话"
        assert xml_tag_inner("<t/>", "t") is None          # 自闭合
        assert xml_tag_inner("<t>没闭合", "t") is None     # 缺闭合标签
        # 实现：v1 同款朴素定位（属性跳过/CDATA 剥壳/自闭合与缺闭合判 None）。
        # 功能：XML 抽取原语锚点。

    def test_unescape_order(self):
        assert xml_unescape("&amp;lt;") == "&lt;"   # amp 最后还原：不产生 <
        assert xml_unescape("&lt;a&gt;&quot;q&quot;") == '<a>"q"'
        # 实现：固定顺序 replace（amp 最后）。
        # 功能：实体还原口径。

    def test_strip_tags(self):
        assert strip_xml_tags("<msg><b>原话</b> 剩余</msg>") == "原话 剩余"
        assert strip_xml_tags("多行\n\n文本   压缩") == "多行 文本 压缩"
        # 实现：状态机剥标签 + 空白压单空格（v1 同款）。
        # 功能：被引用内嵌 XML 转纯文本。


class TestExtractQuote:
    def test_full(self):
        out = extract_quote(_QUOTE_XML)
        assert out == "[引用 张三：被引用的原话] [本条回复：本条回复正文]"
        # 实现：两段文案与 v1 逐字一致。
        # 功能：引用消息渲染主路径。

    def test_reply_only(self):
        xml = "<appmsg><title>只有回复</title><refermsg><displayname>某人</displayname></refermsg></appmsg>"
        assert extract_quote(xml) == "[本条回复：只有回复]"
        # 实现：被引用内容缺失不放弃回复（v1 修复过的坑）。
        # 功能：半引用形态。

    def test_both_empty_none(self):
        assert extract_quote("<appmsg></appmsg>") is None
        # 实现：两段都空才 None（调用方回退 [引用消息] 占位）。
        # 功能：空引用判定。


class TestExtractTitle:
    def test_four_anchors(self):
        assert extract_title('<msg title="标题一">') == "标题一"
        assert extract_title("<msg><title>标题二</title></msg>") == "标题二"
        assert extract_title('<msg des="描述三">') == "描述三"
        assert extract_title('<msg source-displayname="来源四">') == "来源四"
        # 实现：四锚点按优先级。
        # 功能：标题抽取锚点。

    def test_quote_wins_over_lt(self):
        # v1 语义：引号存在时取引号边界，即使尖括号更近
        assert extract_title('<msg title="a<b">') == "a<b"
        assert extract_title("<msg><title>x</title") == "x"
        # 实现：优先双引号边界（与 Rust or_else 链一致），没有引号才取尖括号。
        # 功能：边界语义锚点。

    def test_none(self):
        assert extract_title("<msg>啥都没有</msg>") is None
        # 功能：无锚点返回 None（调用方回退占位）。


class TestRenderContent:
    def test_text_passthrough(self):
        assert render_content({"local_type": 1, "content": "你好"}) == "你好"
        # 功能：文本消息原样。

    def test_text_hex_blob_degrades(self):
        assert render_content({"local_type": 1, "content": "ab" * 30}) == "[卡片/链接消息]"
        # 实现：仍是 hex 形态的 type1 = 解不开的二进制卡片。
        # 功能：降级口径。

    def test_placeholders(self):
        cases = {3: "[图片]", 43: "[视频]", 47: "[动画表情]", 48: "[位置]",
                 50: "[语音/视频通话]", 10000: "[系统消息]", 17: "[合并转发消息]"}
        for lt, expect in cases.items():
            assert render_content({"local_type": lt, "content": "x"}) == expect
        # 实现：占位符文案与 v1 逐字一致。
        # 功能：固定占位锚点。

    def test_quote_and_title_branches(self):
        assert render_content({"local_type": (57 << 32) | 49, "content": _QUOTE_XML}) == \
            "[引用 张三：被引用的原话] [本条回复：本条回复正文]"
        assert render_content({"local_type": (19 << 32) | 49,
                               "content": '<msg title="合并标题">'}) == "合并标题"
        assert render_content({"local_type": (5 << 32) | 49,
                               "content": '<msg title="文件名.docx">'}) == "文件名.docx"
        assert render_content({"local_type": (5 << 32) | 49, "content": "<msg/>"}) == \
            "[链接/小程序/文件分享]"
        # 实现：49 家族按子类型分派（57 引用/19 转发/其它抽标题，抽不到回退占位）。
        # 功能：appmsg 分支锚点。

    def test_voice_asr_and_fallback(self):
        row = {"local_type": 34, "content": "<msg voicelength=\"3440\"/>",
               "voice": {"status": "extracted", "file": "voice_1.silk"}}
        assert render_content(row, transcribe=lambda f: "听写出的文字") == "听写出的文字"
        assert render_content(row, transcribe=lambda f: None) == "[语音]"
        assert render_content({"local_type": 34, "content": ""}) == "[语音]"       # 没导出语音
        assert render_content({"local_type": 34, "content": "",
                               "voice": {"status": "not_local"}}) == "[语音]"
        # 实现：语音分支 = ASR 文本优先，任何失败回退 [语音] 占位（用户拍板）。
        # 解析：transcribe 只在 status=extracted 且有文件名时才调用。
        # 功能：语音渲染锚点。

    def test_unknown_type(self):
        assert render_content({"local_type": 99999, "content": "x"}) == "[非文本消息]"
        # 实现：未识别类型占位不带原始编号（v1 口径）。
        # 功能：兜底分支。
