# grix

[English](README.en.md) | 日本語

索引を持った grep。最初に一度だけツリーの trigram 索引を作り、以後は増分更新
しながら、フルスキャンなら数秒かかる regex 検索にミリ秒で答えます。出力は
ripgrep と行単位で一致します。

[![ci](https://github.com/kyo5uke/grix/actions/workflows/ci.yml/badge.svg)](https://github.com/kyo5uke/grix/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

## なぜ作ったか

- **大きなツリーでは grep は遅い。** ripgrep は素晴らしいツールですが、毎回
  全ファイルを読みます。モノレポでは1検索ごとに数秒、コールドキャッシュや
  ネットワークドライブではもっとかかります。索引ならそのコストは一度きり。
- **実際のワークフローは同じツリーへの繰り返し検索。** リファクタリング、
  コードレビュー、そして AI コーディングエージェント — 1セッションで何百回も
  grep が走ります。grix はその1回1回をポスティングリストの参照に変えます。
- **近似ではなく厳密。** セマンティック検索や embedding ではありません。
  結果は grep が出すものと同じ行・同じ件数で、ただ速いだけ。テストスイートに
  「索引あり検索 ≡ フルスキャン」の性質テストがあり、ベンチマークスクリプトは
  両ツールの出力件数が一致しない限り計測を拒否します。

## 使い方

```
cargo install grix

cd your-repo
grix 'fn main'            # 初回は自動で索引を構築
grix 'fn main'            # 2回目以降はミリ秒
grix index                # pull 後などに増分更新(数秒)
```

デーモンなし・設定なし・モデルダウンロードなし。バイナリ1個。索引は
キャッシュディレクトリ(`%LOCALAPPDATA%\grix`、`~/.cache/grix`)に置かれ、
リポジトリには一切手を触れません。

## ベンチマーク

Linux カーネルソース v6.12(92,823 ファイル、約1.4GB)、Windows 11、NVMe、
ripgrep 15.1.0。[`bench/run.sh`](bench/run.sh) で再現可能。全パターンで
マッチ行数の一致を確認してから計測しています。

| パターン | 一致行数 | ripgrep | grix | 倍率 |
|---|---:|---:|---:|---:|
| `PageTransHuge`(まれなリテラル) | 5 | 2.31 s | 97 ms | 23.7× |
| `EXPORT_SYMBOL`(頻出リテラル) | 38,267 | 2.29 s | 195 ms | 11.7× |
| `static\s+int\s+\w+_probe`(regex) | 10,081 | 2.10 s | 288 ms | 7.3× |
| `spinlock`(`-i`) | 17,086 | 2.23 s | 223 ms | 10.0× |
| `zzqqxx_does_not_exist`(ヒットなし) | 0 | 2.09 s | 41 ms | 50.5× |

索引は 162 MiB、初回構築 約26秒(ファイルシステムキャッシュが冷えていると
約90秒)。無変更時の更新は約2.4秒で、ファイル内容は一切再読しません。なお
計測は Windows(ディレクトリ走査が高コストな環境)です。Linux では ripgrep の
フルスキャンがより速いため、差は縮まります(それでも大きいですが)。

## 仕組み

trigram は連続する3バイトのこと。`hello` を含むファイルは必ず `hel` `ell`
`llo` を含む — だから trigram→ファイルの索引があれば、1GB を読む代わりに
ソート済みリストを数本交差させるだけで済みます。面白いのはこれを任意の
regex でやるところで、grix はパターンを trigram 制約に分解します
(`abc.*def` → `abc` AND `def`、`abc|xyz` → `abc` OR `xyz`)。これは
Google Code Search のために Russ Cox が記述したアルゴリズムです。候補
ファイルには本物の regex を現在の内容に対して実行して確定します。

この最後の点が重要な保証になります: **結果が古くなることはありません。**
マッチは常にライブのファイルから読むため、索引が古くても「最近追加された行を
見落とす」ことはあっても「存在しない行を表示する」ことは絶対にありません。
`grix --explain` で任意のパターンの trigram プランを確認できます。詳細は
[ARCHITECTURE.md](ARCHITECTURE.md)(英語)へ。

## ripgrep 互換性

出力形式(`path:line:text`、tty ではヘッダ形式)、終了コード(0/1/2)、
gitignore の扱い、バイナリ検出、行の意味論は ripgrep に従います。フラグは
いまのところ意図的に最小限です:

| 対応済み | 未対応 |
|---|---|
| `-i`, `-F`, `-l`, `-c`, `-m`, `--json`, `--no-heading`, `--color` | `-A/-B/-C`, `-U`, `-g`, `-t`, `--replace` |

対応パターンで grix と ripgrep のマッチ行が食い違ったらバグです。issue を
ください。

## AI エージェント向け

エージェントは同じツリーを1セッションに何百回も grep します。grix を
使わせれば1回あたりミリ秒です。`--json` で1行1オブジェクトの機械可読出力、
`--stats`/`--explain` でコストの調査ができます。エージェントへの指示に
「コード検索には grep/rg ではなく `grix <pattern>` を使うこと」と書くだけで
動きます(基本的な使い方は引数互換)。

## 先行プロジェクト

- [Google Code Search](https://github.com/google/codesearch) — Russ Cox の
  trigram プランナが本プロジェクトの土台([解説](https://swtch.com/~rsc/regexp/regexp4.html))
- [zoekt](https://github.com/sourcegraph/zoekt) — サーバー型 trigram 検索。
  grix はローカル・ゼロセットアップ版
- [qgrep](https://github.com/zeux/qgrep)、[ugrep-indexer](https://github.com/Genivia/ugrep-indexer)
  — 先行する索引付き grep CLI(アーカイブ式/バッチ索引と、トレードオフが異なる)
- [ripgrep](https://github.com/BurntSushi/ripgrep) — 検索ツールのあるべき姿の
  基準であり、grix のスキャンエンジンが一致すべき相手

## ライセンス

MIT
