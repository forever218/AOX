# textstat

极简文本统计工具：行数、词数、字符数和高频词。纯标准库，无第三方依赖。

这是"计划模式"的一次演练产物：先列 10 个阶段，再逐个执行并在完成后立刻打勾。

## 用法

在项目根目录（`workplace/plan-test`）下：

```bash
python main.py sample.txt                 # 打印报告
python main.py sample.txt --top 3         # 只看前 3 个高频词
python main.py sample.txt --out report.txt
type some.txt | python main.py            # 从 stdin 读
```

报告示例见 `report.txt`。

## 结构

| 路径 | 作用 |
| --- | --- |
| `textstat/core.py` | 统计函数：`word_count` / `char_count` / `line_count` / `top_words` |
| `main.py` | 命令行入口，负责读文件、排版报告、可选写文件 |
| `tests/` | unittest 用例，19 个 |
| `sample.txt` | 示例输入 |
| `report.txt` | `sample.txt` 的实测报告 |

## 规则说明

- `line_count`：末尾换行不算多一行，空文本记 0 行。
- `top_words`：只取字母词（含撇号，忽略数字和标点），统一转小写，默认过滤
  常见英文停用词；次数相同时按字母序排，保证输出稳定。

## 跑测试

```bash
python -m unittest discover -s tests -t .
```

## 演练经过

| # | 阶段 | 结果 |
| --- | --- | --- |
| 1 | 勘察环境 | 读到了上次同类演练的笔记 |
| 2 | 项目骨架 | `textstat/` + `tests/` |
| 3 | `core.py` | 三个基础统计函数 |
| 4 | 基础测试 | 10 个用例 |
| 5 | 首次跑测试 | 1 个断言写错（期望 3 实际 4），改断言后全绿 |
| 6 | `top_words` | 加停用词过滤、稳定排序 |
| 7 | 补测试 | 9 个新用例，19 个全绿 |
| 8 | CLI + 示例 | `main.py`、`sample.txt` |
| 9 | 生成报告 | `report.txt`（13 行 / 127 词） |
| 10 | 收尾 | README、清理缓存、笔记、验证 |
