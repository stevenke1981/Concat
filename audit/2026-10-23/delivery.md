# Concat 改善交付紀錄 — 2026-10-23

本機驗收：PASS。程式版本：`795503a6b1f9015c7b92527caa9e4d85f057408c`。本報告凍結提交／推送前的本機證據；推送後的遠端 SHA 與 CI 由本次對話及外部 `remote-verification.json` 記錄。

## 實際變更

- 合併 `upstream/main` 的 19 筆更新，來源 `1d04d147`，保留 fork 既有 AI／素材庫功能。
- 素材庫新增搜尋：名稱、提示詞、模型與來源；忽略大小寫，可跨欄位比對多個搜尋詞，維持素材 ID 與原有順序。
- 修復去重時刪除唯一剩餘來源檔的風險；已有副本遺失或大小異常時拒絕刪除來源。新素材先複製、保存索引，再刪來源；索引寫入失敗保留來源和舊索引。
- AI／Drive 暫存清理只刪空資料夾，避免呼叫端刪掉尚未入庫的檔案；重新開啟也保留待復原檔。
- 橋接取消／關閉分頁可中止卡住的下載與上傳、釋放工作佇列；已排隊的完成訊息遵守取消與分頁關閉結果。
- 安全模式提示改用可被字串清單工具辨識的翻譯呼叫，保留上游新增的既有翻譯。新搜尋文字提供繁中、英文，其餘語言沿用英文 fallback。
- 完整 Windows 測試把先前的 GPU device-lost 問題定位到 halation 的 FXC X3570／X3511：動態迴圈內的隱含梯度取樣迫使迴圈展開。改用 explicit LOD 0，保留既有色彩空間與數值；僅整合有直接失敗證據的 halation 修正。[Microsoft HLSL 說明](https://learn.microsoft.com/en-us/windows/win32/direct3dhlsl/hlsl-errors-and-warnings)、[W3C textureSampleLevel](https://www.w3.org/TR/WGSL/#texturesamplelevel)。
- Windows 取消測試的假程式改用 `%SystemRoot%` 下的工具完整路徑，且測試明確清空假程式的 PATH，避免它提早退出而誤判取消結果。

## 分工與工具

依 agent-team-workflow 分為整合／桌面、素材庫、橋接三個 owner；兩位作者使用獨立 worktree。Odin 對 `E:/Concat` 執行 live analysis、Rust LSP 符號導覽；權限維持唯讀，建置／測試由 Cargo 和 Node 執行。

獨立檢查只執行一次，針對 `b0b7547c` 找到兩項具體問題，原始結果保存在 [review-b0b7547c.md](review-b0b7547c.md) 與 [findings-b0b7547c.json](findings-b0b7547c.json)。後續由原作者修正及回歸驗證，沒有建立第二輪完整 review，亦不宣稱後續 SHA 已取得獨立核准。

## 驗證

環境：Windows x64、Rust 1.93.1、Slint 1.18.1；Cargo 工作目錄 `src/`，使用既有 `E:/concat-deps/env.sh` 與 `CARGO_TARGET_DIR=E:/concat-deps/target-app`。

| 檢查 | 結果 | 證據 |
| --- | --- | --- |
| 素材庫 regression 舊碼重現 | FAIL（預期） | 14 通過、4 失敗，來源檔不存在，exit 1 |
| 修復後素材庫測試 | PASS | 18 通過，exit 0 |
| 桌面搜尋初次測試 | PASS | 176 library + 5 main，exit 0；後續追加復原測試另列全套結果 |
| 最終桌面／復原測試 | PASS | 全工作區執行中 178 library + 5 main 全過，含清理及重新開啟不刪未入庫來源 |
| 橋接舊碼與 queued-done 重現 | FAIL（預期） | 取消後未中止下載；分頁關閉被誤報 done，exit 1 |
| 橋接修復後測試／語法 | PASS | 主工作樹 37 測試通過，`npm run check` exit 0 |
| Format、locale、model inventory | PASS | 1008 字串，無 missing/stale；23 models；exit 0 |
| 全工作區 Clippy | PASS | `cargo clippy --locked --all-targets -- -D warnings` exit 0；後續 shader／fixture 另跑受影響檢查 |
| 全工作區初測 | FAIL，失敗項目已修復並重測 | 1140 通過、3 失敗、3 忽略；保留初次 exit 1。失敗是 host 的 GPU 卡片／取消假程式與 render shader probes |
| 修復後 host／GPU／引擎測試 | PASS | 603 通過、0 失敗、0 忽略，exit 0；含實際 GPU 與匯出。其餘初測通過部分未重跑完整套件 |
| 修復後受影響 Clippy | PASS | host／effects／export／render／project，all-targets、GPU、`-D warnings`，exit 0 |
| 最終 app check | PASS | `cargo check --locked -p concat` exit 0 |
| 原生 build／smoke | PASS | 最後 build exit 0；最後執行檔 `--smoke` 初始化 RX 6800 XT、視窗元件與 Whisper，`smoke: ok`、exit 0 |
| 原生視窗啟動／關閉 | PASS | 隔離資料夾、實際 `Concat` window handle、8 秒存活、無 runtime error，關閉後 exit 0；見 [native-window.json](native-window.json) |
| GUI 畫面／實際搜尋互動 | NOT_RUN | 此工作階段沒有可用桌面操作工具；AutoGui runtime 設定指向的 EXE 不存在 |
| 真實 Flow／ChatGPT／OpenRouter／Drive | NOT_RUN | 未操作帳號或消耗生成額度 |
| 遠端 push／CI | NOT_RUN（本機驗收時點） | 已授權推送既有公開 fork `stevenke1981/Concat` 的 `main`；完成後另查 exact-HEAD 結果，不把排隊當成功 |

完整本機紀錄：`E:/concat-deps/optimization-20261023/`。檔案與紀錄 SHA-256 見 [verification-sha256.json](verification-sha256.json)，交接欄位見 [handoff.json](handoff.json)。原生 smoke 觀察到此環境沒有音訊輸出裝置，實際聽感驗證為 NOT_RUN。

已確認執行檔：`E:/concat-deps/target-app/debug/concat.exe`，SHA-256 `c7d4cc7add90a669edc33a6effab67f90a500be447753482b34e56c486ad3c79`。

兩項獨立檢查發現已分別由 `1851e525`（queued-done 的關閉結果）與 `a754c26e`（未入庫檔案清理／重啟）修復，對應 regression 均通過。額外的 Windows FXC 與 PATH fixture 修正為 `d2a2027f`、`795503a6`，三個完整初測失敗項目皆在 603 個相關測試中通過。

主要重現命令（Cargo 均在 `src/`）：

```text
cargo fmt --all --check
python scripts/locales.py --check                # repo root
python scripts/models.py --check                 # repo root
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --workspace --no-fail-fast    # 初測，保留失敗紀錄
cargo test --locked -p concat-host -p concat-effects -p concat-export -p concat-render -p concat-project --features concat-render/gpu --no-fail-fast
cargo clippy --locked -p concat-host -p concat-effects -p concat-export -p concat-render -p concat-project --all-targets --features concat-render/gpu -- -D warnings
cargo check --locked -p concat
cargo build --locked -p concat
npm test                                        # extensions/concat-bridge
npm run check
```

## 限制

- 去重副本檢查使用 metadata，不另外重算大型影片 SHA；同大小的外部內容損壞不在此次涵蓋範圍。
- move 為保護來源增加一次複製；失敗後的暫存檔需復原或手動清理，不會自動刪除。
- 保留使用者原有未追蹤 `.agents/` 和 `result00.md`；不納入提交。不建立 release、tag 或部署。
