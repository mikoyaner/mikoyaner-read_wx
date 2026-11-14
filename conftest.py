"""pytest 根配置：把项目根放进 sys.path，让 tests/ 直接 import wxmsg 与 keyscan_readonly。

无需 __init__.py 链——pytest 发现根目录有 conftest.py 时会自动把根目录插入 sys.path[0]。
"""
# 实现：空实现的根 conftest，仅靠 docstring 说明其存在意义。
# 解析：tests/ 与 wxmsg/、keyscan_readonly.py 是平级目录，若没有根 conftest，pytest 的 rootdir
#       不会自动进 sys.path，import wxmsg 会 ModuleNotFoundError。
# 功能：让 `pytest` 在项目根直接可跑，测试内 import 不依赖安装步骤。
