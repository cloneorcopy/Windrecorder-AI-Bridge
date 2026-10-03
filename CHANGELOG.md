# Changelog 更新日志

## 未发布 unreleased — 「停止整理」现在真的是立刻停止 stop means stop now, the encoder included

- 原来的规矩是"手上的那件做完就退"。对识别一行、对一段缩略图那都无所谓，但 `convert` 手上的一件是**一整段录像的 ffmpeg 编码**——一片长的能占一分钟，`expire` 的压缩同一件也是整段重编——所以按了停止之后，屏幕上一秒都不动，看上去就是"停不了"。使用者的话很直接：不是等它做完，是马上放下。
  The old rule was "finish the item in hand". For a row of text or one preview that costs a fraction of a second, but the item in `convert`'s hand is **one whole segment's ffmpeg encode** — a long one is a minute — and `expire` re-encodes a whole segment too. So the button was answered a minute late, which is indistinguishable from not being answered at all.
- 编码器现在被**打断**：`layout::run_ffmpeg` 不再 `.output()` 死等，改用这条 pass 自己那套 `schedule::Child`（两条管道都有人读、一秒问一次"还能不能干"、`kill` 之后 `wait`）。被放下的那一片：写到一半的输出文件删掉、切片目录保持未标记、不改任何索引行——下一轮从同一批帧重做。这条清理不是附加品，它是允许打断的理由：ffmpeg 直接写视频的**最终文件名**，留下半截文件就是让下一轮把截断的一小时当成别人的成品（`AlreadyPresent`）。
  The encoder is interrupted now: `run_ffmpeg` hands the child to `schedule::Child` — the pass's own "read both pipes, ask once a second, kill and wait" — instead of `.output()`, which cannot be interrupted at all. A put-down encode deletes the partial output, leaves the slice unmarked and touches no index row, so the next pass re-encodes from the same frames. That cleanup is not a courtesy, it is what makes the kill legal: ffmpeg writes the video's *final* name, and a truncated file left there is read by the next pass as somebody else's finished hour.
- 被停止放下的编码**不算失败**：`Did::CalledOff` / `CompressResult::CalledOff`，报告行多一列 "N put down by the stop"。以前它落进 `failed`，于是这一步会回答 "N slice(s) could not be encoded; they are left unmarked" —— 把一个人按的按钮写成机器的故障，是让下一个读日志的人去找不存在的编码器。
  A put-down encode is not a failure: it gets its own variant and its own column, because counting it with the broken ones makes the step answer *"N slice(s) could not be encoded"* about somebody's stop button.
- 按下去的第一秒就有话：`wind_base::maintain::stopping()` 在读到停止标记之后**立刻**把进度写成 `stop requested — putting the work in hand down (step N of 9)`，然后才去等腿收干净，最后 `finish` 换成带件数的结束句。`stopping` 不写 `finished`、不关任何一条腿、不认领任何一件——那些要等真正知道的时候再说。界面的条也不再有"闪给一个已经被叫停的轮次看"的余地：`is_running()` 读到的就是 `stopped`。
  The button is answered in the same second it is read: `stopping()` publishes *"stop requested — putting the work in hand down (step N of 9)"* before the pass waits for anything, and `finish` replaces it with the numbered ending. It writes no `finished`, closes no leg and claims no item — those belong to the ending. And because the state word changes, `is_running()` is false, so no bar keeps breathing for a pass that has already been called off.
- 还剩一件不是秒级的：缩略图那一帧的读取（`previews` 自己的 ffmpeg，实测 0.5–5.7 秒）没并进这次改动，它不在同一条等待上。要一并打断是下一步的小改。
  One item is still not sub-second: the preview's own one-frame ffmpeg read (measured 0.5–5.7 s) is a separate call and was not folded into this change.
- 测试：`the_stop_is_answered_before_the_pass_has_stopped_waiting`（立刻变 `stopped`、不带 `finished`、腿一根都不关、`is_running()` 为假，之后结束句替换它）、`a_running_encoder_is_put_down_when_the_pass_is_called_off`（真的把一个睡 30 秒的子进程在一秒的轮询里放下，答案是 `Called::Off`）、`an_encoder_that_fails_is_not_reported_as_an_encoder_that_was_stopped`（坏状态码是 `Failed` 且带 ffmpeg 自己的话；没有这个二进制也是 `Failed`）、`a_put_down_encode_is_counted_apart_from_one_that_failed`。`cargo test -p wind-maint`：**175 绿**；`cargo test -p wind-base`：170 绿 / 1 红（那份新检出按 CRLF 读出厂提示词文本的逐字比对，主树为绿）。
  Four tests, including a real child asleep for thirty seconds put down inside the one-second poll. `cargo test -p wind-maint`: **175 passed**; `wind-base`: 170 passed with the single CRLF-checkout failure that is green in the main tree.

## 未发布 unreleased — 两条在实机上看穿的谎话：AI 那行不许说"还没开始"，也不许许诺一轮干不到的数 two rows the live pass caught lying: the AI row may not say "not started" and may not promise what one pass cannot reach

- 装了新版之后按"立刻整理"真跑了一轮（12:10 起，`kind=manual`，录制器全程在录）。进程表每秒采一次：`text` 16 秒、`convert` 6 秒、`expire` 1 秒、`previews` 3 秒，`ai-summaries` 一直到 12:19 还在跑；**12:10:33–12:10:37 那 4 秒里，`windai.exe`（网络）、`ffmpeg.exe`（编码器）、`wind-reindex.exe`（识别引擎）三个同时活着**，总量条 62+3+137+27 由普查一次数定。中午的活本来就少（切片 3 片、行 62 条、需要补索引的段 0 个），所以重叠只有 4 秒可看——这不是设计不出来，是今天没有那么多本地活可压；夜里那轮（3799 行 / 70 片）这三段本来各自都是几十分钟。
  A real pass on the installed build (started 12:10 from the button's own signal, recorder capturing throughout) was sampled once a second: `text` 16 s, `convert` 6 s, `expire` 1 s, `previews` 3 s, and the AI leg still going at 12:19. **For four seconds, 12:10:33–12:10:37, `windai.exe` (the wire), `ffmpeg.exe` (the encoder) and `wind-reindex.exe` (the engine) were all alive at the same instant.** Noon simply has little local work — three slices, 62 rows, zero segments needing a back-index — so four seconds is what there was to see; the night's queue is 3799 rows and 70 slices, each of which was tens of minutes on its own.
- 那一轮把两处谎话照出来了。**第一处**：AI 那行在整个第一条请求在路上期间都写着"还没开始"（`leg.ai.state=waiting`，而 `windai.exe` 明明活着）。这个端点一条要一分多钟，所以那是一分多钟的假话。现在开局起线程、三道门都过了、确实有已定稿的日子要问的时候，先 `report_leg(Ai, Running, "settled days, asked under the local steps")`——只说在问，不认领任何一件还没回来的。
  **The first lie**: its row read "not started" for the whole of the first request (`leg.ai.state=waiting` while the process table showed `windai.exe` alive). This endpoint takes over a minute a stretch, so that was a minute of a false sentence. The half now says it is asking the moment the three guards have passed and there is a settled day to ask — and claims no item, because none has come back.
- **第二处**：AI 那行的分母是普查数出来的**待总结全部**（137 段），而设置页规定一轮只接手 `summary_stretch_limit_in_idle` 条（这台机器上是 40）。于是那根条永远到不了头，看着像卡住。现在分母 = min(待总结, 本轮额度)，差额写在这一行的 note 里（"N stretch(es) are outside one pass's ceiling; they wait for a later pass"）；本地三条腿仍用普查原数，"数一数"面板答的也还是整个待总结清单——两个问题两个数，各自说清楚。
  **The second lie**: the row's denominator was the census's whole pending backlog (137 stretches) while the settings page caps one pass at `summary_stretch_limit_in_idle` (40 here), so the bar could never close and read as a stall. The denominator is now the pass's own ceiling and the remainder is said on that row instead of being left as a gap nobody explains. The local legs keep the census's counts, and the 数一数 panel still answers "what is waiting" with the whole queue: two questions, two numbers, each labelled.
- **第三处，也是最贵的一处**：一轮把设置页的额度花了**两遍**。`summary_stretch_limit_in_idle` 说的是"单次空闲总结接手的片段数"（这台机器 40），可拆分之后开局那一半先发 9 条、第 8 步又发一次 `--limit 40`，补发再看一次 `--limit 40`——同一轮最多 80 条，设置那一行说的话不算数。现在额度是一轮一份：第 8 步只花 `room_left(额度, 开局已发)`（实机这一轮就是 40-9=31），**补发只问欠着的那几件**（`--limit owed`，回到使用者定的"仅重试一次失败项"），额度花完就直接说"本轮额度已用完，其余等下一轮"而不是再开一张账单。实机的 argv 就是证据：改之前同一轮里出现两次 `--limit 40`。
  **The third and costliest lie: one pass spent the settings page's ceiling twice.** `summary_stretch_limit_in_idle` says how many stretches one idle run takes on (40 here), yet after the leg was split the early half sent nine, step 8 then sent `--limit 40` again, and the retry sent `--limit 40` a third time — up to eighty in a pass, which is not what that row promises. The ceiling is now one per pass: step 8 spends `room_left(ceiling, asked_by_the_early_half)` (31 on this run), the retry asks only the debt (`--limit owed`, back to the owner's 仅重试一次失败项), and a spent ceiling says so instead of opening a second bill. The live argv is the evidence: two `--limit 40` in one pass before the fix.
- 新增 3 条测试：`the_ai_leg_says_it_is_working_before_its_first_answer_lands`（开局是 waiting、`announce_asking()` 之后是 running 且 done 仍为 0、没装发布者就不改写别人的文件）、`one_pass_promises_its_ceiling_and_names_what_it_leaves_out`（137/40 → 许诺 40、差额 97、额度抬到 200 就许诺整个 137）、`the_two_halves_spend_one_ceiling_between_them`（40-9=31、花满是 0 而不是回绕、补发带的数字是欠账数）。`cargo test -p wind-maint`：**172 绿**。
  Three tests: the row goes waiting → running claiming nothing, and an uninstalled pass rewrites nobody's file; 137 under a ceiling of 40 promises 40 and names 97, while a ceiling of 200 promises the whole 137; and 40 less nine spent early is thirty-one, a spent ceiling is zero rather than a wrapped-around limit, and the retry carries the debt. `cargo test -p wind-maint`: **172 passed**.

## 未发布 unreleased — 三条真正占时间的腿第一次同时跑，进度改成人看得懂的四根件数条 the three things that cost time now run at once, and the progress became four item bars a person can read

- **AI 腿不再排在最后**：整理一开局（分母刚数完）就起一条线程，`schedule::ask_settled` 只问"本轮已经改不动的那几天"。什么算改不动，写在 `day_is_settled` 的三个条件里：当天的不算（切片还在进、`text` 还在写它的行）、自己的切片目录还在磁盘上的不算、`expire` 的保留期能够得着的不算，而且每一帧都得已经有文字。问出来的账不在线程里打印，`EarlyReport` 交回第 8 步 `ai_summaries_after`，在它自己的边界上说一句，剩下的日子仍由那一步按老问法索取，**一轮只补一次**的判定跨两半的欠账一起算。这条腿仍然只起一个 `windai`：那一个进程自己已经在飞 4 条请求，再起一个就是 8 条，而 ADR 把宽度定在 4——这条腿挣的是与本地腿的重叠，不是与自己的。
  **The AI leg stopped being the last thing in the pass**: as soon as the pass fixes its denominators, a thread asks only the days this pass can no longer change. `day_is_settled` names three reasons a day is not settled — it is today (slices still arriving, the text step still writing its rows), its own slice folder is still on disk, or the retention sweep can reach it — and every frame must already carry text. The lane prints nothing: its `EarlyReport` is handed to step 8, which says the line at its own boundary, still asks the remaining days the way it always did, and decides the pass's single retry over **both** halves' debt. One `windai` only, because that process already keeps four requests in flight and a second would put eight on a wire the ADR fixed at four — the overlap this leg buys is with the local legs, never with itself.
- **合成与补索引按段接上**：`convert` 每把一片改名成 `-VIDEO`，就 `Handoff::mark_encoded` 交给追在后面的那条腿（`schedule::Follow`），凑够 8 片起一个 `wind-reindex`（引擎冷启动 29–46 s，少于一批就不起）。于是编码器还在合后面的小时，识别引擎已经在读前面那几片。合成一结束，`Follow::finish` 在**第 2 步的边界内**等这条腿收干净，绝不跨过 `refresh`/`expire` 去开同一本月份库——同一时刻一本库仍只有一个写者，`busy_timeout` 是撞门时的兜底，不是被拿来当写锁用。`--dry-run` 一条腿都不起：没有线程就没有人能起子进程、发请求、写或改一个名字，测试 `a_dry_run_pass_starts_no_leg_and_claims_nothing_it_did_not_do` 就是拿两株假 `MZ` 产物和整棵目录树比对来证明这句话的。
  **Encoding and back-indexing now meet segment by segment**: every slice `convert` renames to `-VIDEO` is handed to the lane behind it, and eight of them start one `wind-reindex` (an engine costs 29–46 s cold, so a smaller batch is not worth a process). The encoder is still cutting later hours while the recognition engine reads the earlier ones. When the step is over, `Follow::finish` joins the lane **inside step 2's boundary**, so `refresh` and `expire` never open a month database that somebody else is still committing to: one writer per file is unchanged, and `busy_timeout` stays the fallback for readers rather than being called a lock. A dry run starts no leg at all — with no thread nothing can spawn a child, send a request or rename a byte, and the test proves it by planting two files whose contents are the bytes `MZ` and comparing the tree afterwards.
- 为此 `wind-reindex` 多了一个可重复的 `--file`：一次交一批已经标了 `-VIDEO` 的段，而不是让它去重走月份目录。批次用与走目录**同一个** `order_by_stamp` 排成最旧在前，因为分道与"这一道没活"的报告都定义在那个次序上；库根取本安装的 `userdata\videos`，所以从数据库行或命令行来的名字仍然伸不到别的录像外面。`--file` 与"什么都不指名就是拒绝"各有测试，帮助文本里那条"每个选项都要有文档"的守护也一并把 `--shard`、`--file` 收了进去。
  `wind-reindex` gained a repeatable `--file` for exactly this: a batch of already-marked segments instead of a re-walked folder. The batch is sorted oldest-first by the *same* `order_by_stamp` a directory walk uses, because the lane deal and the "this lane has nothing" report are both defined on that order, and the containment root becomes the install's own videos library, so a name from a database row still cannot reach sideways. Tests cover the batch parse, the ordering, and the refusal when nothing was named; the guard that every accepted option must be documented now checks `--shard` and `--file` too.
- **一条新的安全边界**：一个视频旁边的切片目录如果**还没标记**，这一步就把它延后，什么都不改名（`still_in_the_encode_queue`）。`convert` 是先写最终文件名、ffmpeg 回来之后才把切片改名 `-VIDEO` 的，所以"文件名已经在了而切片还没标记"是"这个文件可能还在长"的正面证据——读它会只 OCR 到碰巧在的那部分然后把整小时标成读完，剩下的字就永远没了。追合成的腿与编码器同时跑，靠的就是这一条；帮助文本里写明，两侧各一条测试（未标记 → 延后且不动；标了 `-VIDEO` → 照旧干活）。
  **A new safety boundary**: a video whose slice folder still sits beside the cache *unmarked* is deferred, with nothing renamed on it. `convert` writes the final name first and marks the slice `-VIDEO` only after ffmpeg returned, so an unmarked slice is positive evidence the file may still be growing — reading it would OCR the part that happened to be there, mark the whole segment done, and lose the rest of the hour. This is what lets the lane follow the encoder at all; the help text says so, with one test on each side of the rule.
- **给人看的形状换掉了**：设置页不再显示"第几步 / 共九步"，也不显示秒数。上面一根总件数条，下面四条腿各一根（文字识别 / 视频合成 / AI 总结 / 其他整理），每根都是 `已干完 / 本轮数定` 加一句人话；某条腿本轮 0 件就不画那一行；本地出错才变红，接口没回话是黄，**网络类失败不算整理失败**。九步在命令行与 `PROGRESS.MD` 里仍然是九步（诊断、手动单步、以及只认 `step` 的老读者都还要用），只是不再是给人看的形状。文案三语（en/sc/ja）在 `copy.ts` 与 `config_src/languages.json` 两边对齐，守护测试读的是原表。
  **The shape a person is shown changed**: the settings page no longer says "step N of 9" and no longer counts seconds. There is one total bar and one per leg (screen text / videos / AI summaries / the other tidying), each `done / the number fixed at the start` plus one plain sentence; a leg with nothing this pass draws no row; red means this machine could not do its own work and amber means an endpoint that has not answered, so a quiet AI leg cannot redden the pass. The nine steps remain in the command line and in `PROGRESS.MD` — diagnostics, single steps and any reader that only knows `step` still need them — they are simply no longer the shape a human is shown. Copy is in three locales, kept aligned between `copy.ts` and `config_src/languages.json` by the guards, which read the raw tables.
- 一处口径修正：AI 那条腿数的是**待总结的片段数**（普查里的 `summaries.stretches`，入账的 `add_items(Leg::Ai, written)`），三语文案原本却写着 `days / 天 / 日`。现在改成 `stretches / 个片段 / 区間`，ADR 里那根示例条也跟着改。分母与分子是同一件事，这件事只有写它的人说得准，所以两处都由普查回答。
  One unit was wrong: the AI leg counts **pending stretches** (`summaries.stretches` in the census, `add_items(Leg::Ai, written)` on the way in) while its three locales said `days / 天 / 日`. They now say `stretches / 个片段 / 区間`, and the ADR's sample bar was corrected with them. A denominator and a numerator have to be the same thing, so both are answered by the census rather than by whoever draws the row.
- 第四处（窗口侧）：一轮被叫停之后，`PROGRESS.MD` 里那条腿仍然写 `running`——发布者已经不写了，没有谁会替它把这句说圆。于是设置页上那根条在"上一轮在第 8 步后收手"底下继续呼吸，等于窗口在声称几小时前就停了的活还在干。改成腿的闪动只跟**整轮**的是否在跑（`progress.running`，上面总量条用的同一个词，而且是拿进程表对过的答案），文件里的 `running` 单独不再让条闪。`tsc --noEmit` 干净。
  **The fourth, on the window's side**: after a pass is called off, its leg row is still `running` in `PROGRESS.MD` — the publisher has stopped writing, so nobody is left to say the polite end of that sentence — and the bar kept breathing under "上一轮在第 8 步后收手", which is the window claiming work that stopped hours ago. A leg row now pulses only while the **pass** is running (`progress.running`, the same word the total bar above already reads, and one that is answered against the process table), so a file's stale `running` cannot make a bar breathe. `tsc --noEmit` clean.
- 门禁：`cargo check --offline --workspace --all-targets` 干净（只剩 `windui`/`windrec` 原有的 dead-code 警告）；`cargo test --no-fail-fast` 逐个 crate：**wind-maint 169 绿、wind-reindex 112 绿、wind-store 77 绿、wind-summary 61 绿、windui-web 34 绿、wind-base 169 绿 / 1 红、wind-ai 146 绿 / 3 红**。四条红全是这份新检出的工作树按 CRLF 读出厂提示词文本导致的逐字比对（`prompts::rendering_fills_every_slot…`、`prompt::shipped_text_moved…`、`prompt::the_tag_table_keeps…`、`tags::the_table_is_csv_with…`），主树同一提交为绿。`tsc --noEmit` 干净。决定与"这一轮到底落到哪了"记在 `docs/adr/2026-09-30-the-organise-pass-runs-on-four-legs.md`：腿一的 `text` 多引擎并行与"每个月库一把写锁"这两条**没做**，各自的替代写法也写在了那里。
  The gates: `cargo check --offline --workspace --all-targets` is clean apart from the `windui`/`windrec` dead-code warnings that were already there, and the tests per crate come back **wind-maint 169, wind-reindex 112, wind-store 77, wind-summary 61, windui-web 34, wind-base 169+1, wind-ai 146+3**. The four failures are this fresh checkout reading the shipped prompt text back with CRLF and comparing it byte for byte, green in the main tree. `tsc --noEmit` is clean. The ADR records the plan and, honestly, where this round actually landed: multi-engine `text` and the per-month write lock are **not** in, and what was done instead of each is written beside them.

## 未发布 unreleased — AI 腿四条请求同时在飞、回忙降流、失败按波计、判失败前先认盘 the AI leg runs four at once, slows on a busy answer, counts failure by wave, and checks the disk before calling anything missing


- `windai summarize` 的段总结不再一条一条等：一波同时发 **4 条**请求（`IN_FLIGHT`，写死在代码里并说明为什么是四条，不新增设置键、不新增设置页那一行）。**一条仍然只问一段**——把几段合进一个请求的做法在使用者那里已经被否掉，回话对不上号的风险不该由一次网络往返承担。每条独立入账，哪条没回只欠那一条。请求在线程间怎么交错都不影响报告：结果按条目序折叠（`wind_base::pool` 的既有规矩），`--dry-run` 一字不改地照旧不发不写，`--force`/`--limit`/`--pending` 与每日计数语义全部保留，收尾那句 `"{sent} request(s), {written} written, {cached} current without asking, {failed} failed, {chars} characters total"` 一字未动，因为 `windmaint` 的补发判定就从里面读失败数。
  The stretch pass no longer waits one request at a time: a wave puts **four** on the wire at once (`IN_FLIGHT`, pinned in code with the reason written beside it — no new config key, no new settings row). **One request still asks about one stretch**; folding several stretches into one call was refused by the owner, since an answer that cannot be matched back to its own stretch is worse than a slow one. Each request is accounted for alone, so the one that did not come back owes only itself. How the threads interleave cannot leak into the report: answers are folded in item order, the rule `wind_base::pool` exists for, `--dry-run` still sends and writes nothing, `--force` / `--limit` / `--pending` and every per-day counter keep their meaning, and the closing sentence is untouched byte for byte because `windmaint` reads the failed count out of it.
- **自动降流**：对方回"忙"——状态码 429 或 503——本轮之后每波只发 **2** 条，本轮不回升，下一轮从 4 条重新起。判据读的是传输层已经带出来的状态码（`AiError::is_busy`），不是错误文本：拿句子去猜"服务繁忙"还是"配额用尽"，会让一次并不存在的限流把整晚的节奏改掉，而 401、402、404 这种"永远不行"更不该被当成等一下就好。
  **Auto-throttle**: when the endpoint answers *busy* — status 429 or 503 — the rest of this pass runs two at a time and does not climb back within it; the next pass opens four again. The predicate reads the status number the transport already carries (`AiError::is_busy`) rather than the wording of a body, because deciding from prose would slow the whole night for a rate limit that does not exist, and a 401/402/404 that means *never* is the opposite of busy.
- **失败按波计**：一整波同时没回话只算 **1** 次"这条不通"（`AiError::is_silent`，只数传输层的失败：截止线到了、socket 断了、握手没成），不是 4 次；连续 3 波全灭，这条腿整条收住，报告写"接口没回话，还剩 N 件没问"，没收问的那几件仍然是欠着的（既不算写成，也不再发第二遍）。这个门是照识别引擎的 `wxocr::GIVE_UP_AFTER` 的形状加的——那里也是三，只是这里的单位是波。没有它，拉长超时之后再并发四条，等于一波全灭就当场收腿。
  **Failure is counted per wave**: a whole wave coming back silent is **one** "this did not come back" event (`AiError::is_silent`, transport-level only — deadline, socket, handshake), not four. Three silent waves in a row close the leg, the report says how many stretches it then left unasked, and those stretches stay owed: not written, and never sent a second time. The gate is modelled on the OCR engine's `wxocr::GIVE_UP_AFTER` — also three, only here the unit is a wave — and without it, four-in-flight plus a fifteen-minute deadline would turn one bad wave into an instant give-up.
- **判失败之前先认盘**：超时的请求可能对方其实已经写完——MCP 桥写的是同一批按产品日归档的 JSON。每条腿收尾时先用 `wind-summary` 现推的队列复核一次（`digests()` 与 `for_day_with`，同一段的账只能有一套数法），凡提示词指纹与内容指纹都已经对上的，一律算完成、不发第二遍，也不新增一个"别人写了几件"的计数：它落进原来的 `current without asking` 里。`--force` 重问的那些不参与这个判定，它们开工时就不欠着。
  **The disk is asked before anything is called missing**: a request that timed out may already have been answered by the MCP bridge, which writes the same per-day JSON. Each day closes with a re-read of the queue derived through `wind-summary` (`digests()` and `for_day_with`, so one ledger is counted one way), and anything whose content and prompt digests now match counts as done with no second request. No new counter is invented for it — it lands in the existing `current without asking`. Stretches re-asked by `--force` are excluded, because they owed nothing when the pass started.
- 并发的两处落地细节：`Client<T>` 不对 `T: Transport` 承诺 `Sync`，所以没有给它补 `unsafe impl`——每条腿为手里那条请求从同一份 `Settings` 建一个自己的 client（`WinHttp` 本来就是每个请求开一个会话，共享一个 client 等于共享一份配置副本而已），请求在主线程渲染好再交出去，省掉腿里再读一次模板的等待。写盘继续走 `write_period` 那两把门（进程内互斥 + pid 锁），四条腿同时写同一天也不会丢段；这条路径不写 SQLite 索引，`wind-summary` 只读月库，所以没有需要留在主线程的索引写入。
  Two implementation details of the concurrency: `Client<T>` promises nothing about `Sync` for its transport, so no `unsafe impl` was added — each lane builds its own client from the same `Settings` (`WinHttp` already opens a session per request, so a shared client would have shared nothing but a settings copy), and requests are rendered on the calling thread before being handed out, so no lane waits on a template read. Writes still go through `write_period`'s two gates (a process mutex and a pid lock), so four lanes writing one day lose no paragraph; and this path writes no SQLite index at all — `wind-summary` opens the month databases read-only — so there was no index write to keep on the calling thread.
- 新增 5 条测试（`ai/src/summarize.rs`，用进程内的假传输）：四条在飞的回话各自落在自己那段、429 之后本轮每波只发两条、一波四条全没回话只算一次不通、连续三波全灭之后剩下的不再问、以及超时那条其实文件已在盘上时算完成而不算失败。`cargo test --offline -p wind-ai`：**141 绿 / 3 红**，三条红仍是这份新检出的树把出厂提示词文本按 CRLF 检出导致的逐字比对（`prompt::tests::shipped_text_moved_without_changing_a_single_byte`、`prompt::tests::the_tag_table_keeps_upstreams_column_names`、`tags::tests::the_table_is_csv_with_upstreams_column_names_and_longest_focus_first`），主树同一提交为绿。`wind-maint` 的 `owed` 与它的测试一字未改。
  Five tests added in the crate's own style against the in-process fake transport: four answers in flight land on the four stretches they were asked for, a 429 drops the rest of the pass to waves of two, one silent wave of four is one failure, three silent waves leave the rest unasked, and a timeout whose file was written meanwhile counts as done rather than failed. `cargo test --offline -p wind-ai`: **141 passed, 3 failed** — the three are this fresh checkout reading the shipped prompt text back with CRLF and comparing it byte for byte (`prompt::tests::shipped_text_moved_without_changing_a_single_byte`, `prompt::tests::the_tag_table_keeps_upstreams_column_names`, `tags::tests::the_table_is_csv_with_upstreams_column_names_and_longest_focus_first`), green in the main tree. `windmaint`'s `owed` and its test are untouched.

## 未发布 unreleased — AI 腿：等得久一点、一次问四条、一轮里补一次 the AI leg waits longer, asks four at once, and gets one retry inside the pass

- 出站超时按使用者的决定拉长：解析 5→10 秒、连接 10→15 秒、发送 60→300 秒、**接收 180→900 秒**。理由写在当晚的日志里：一次要总结四十段，其中两段是 `accepted the connection but sent no reply (WinHTTP error 0x00002EE2)` ——那是接收截止线到了，而线那头是一个使用者自己的排队推理服务。代价同时讲明白：一条最多能占十五分钟，所以到点收尾可能超出窗口十几分钟，因为"还能不能干"只在两件之间问，不掐正在发的那条。
  The outbound deadlines go up on the owner's instruction: resolve 5→10 s, connect 10→15 s, send 60→300 s, **receive 180→900 s**. The reason is in that night's own log — two of forty stretches answered `accepted the connection but sent no reply (WinHTTP error 0x00002EE2)`, which is the receive deadline expiring on a queue the product does not control. The cost is named rather than hidden: one request can now overrun the closing of the maintenance window by up to its own deadline, because the pass asks "may I still work" between two items and never in the middle of a sent one.
- 一轮整理跑完之后，**如果窗口还开着、没人叫停**，本轮没送到的那几件再问一次；补发仍然失败的写"两次都没送到，等下一轮"，这一轮之后不再自动碰它。补发不用"失败清单"：欠着什么由账和提示词指纹现推，已经落地的自然不会被重问。判定读总结器自己那句收尾话里的失败数（`… current without asking, N failed …`）——再造一套"还欠几件"的数法，就是让整理和工具各说各话。没设窗口的安装拿不到第二次，因为那次补问的存在理由就是"今晚的窗口还没走完"；被叫停也不补。
- 决定与后果记在 `docs/adr/2026-09-30-the-organise-pass-runs-on-four-legs.md`：四条腿并行（识别 / 按段流水 / AI / 收尾杂项）、进度只数件数（四根条，不折算时间）、reindex 不再重读录制器读过的段、以及**明确否掉**的三件事——按秒折算份数、把多段合进一个请求、给补发加成本闸。同一条腿还定了：同时在飞四条请求（一条仍只问一段）、对方回忙就本轮降到两条、失败按"波"计（同波全灭算一次）、连续三波不通这条腿收住。
  A pass now asks once more, in the same pass, for the items that did not come back — but only while the window that scheduled it is still open and nobody has asked it to stop; a second miss is reported as `still missing after one retry` and waits for the next night. No failed-key list is carried across the two runs: what is owed is derived from the index and the prompt digests, so whatever landed is simply not asked again. The count comes from the summariser's own closing sentence, because a second way to count the same queue is how a pass and a tool start disagreeing about whether anything is left. The same page decides the rest of the leg: four requests in flight at once, one stretch per request (merging stretches was weighed and refused), a busy answer drops this pass to two in flight, and a whole wave failing counts as one failure rather than four.

## 未发布 unreleased — 整理不再把同一小时读两遍，一步之内多件事同时做 the pass stopped reading one hour twice, and a step now works on several things at once

- 先记下这一轮实测的账：手动整理一轮（`kind=manual`，01:11 起）里 `text` 用了 23.9 分钟读 3799 行，`convert` 用 7.6 分钟编码 70 个切片（每片一次 ffmpeg），`refresh` 7 秒，`expire` 不足 2 秒，`reindex` 跑了 40 分钟以上还没完。窗口是 01:30–05:00，而九步全在一根线程上排队。
  `reindex` 那一步在重复劳动：录制器为每张留下的画面写一行，`text` 读的是那一行的掩码副本，等到 `convert` 把切片编码成视频，同一小时的画面已经被读过一遍；`reindex` 仍按 4 秒抽帧把这段视频再 OCR 一次——949 分钟的录像约 14236 帧。更糟的是它动手之前先 `rollback`，按段名删掉录制器写的那些行，换成自己从视频里抽的粗一行。普查当晚的库：09-25 到 09-28 的行全部是 `N_cropped.jpg`（reindex 的形状），09-29 起全部是时间戳命名的录制器行，同一小时两种行同时存在的段是 0；等到这一轮跑到 09-29 的段，那五个录制器的行被删掉、新行是 0 条。
  现在的规矩写在 `wind_store::maintain::segment_coverage`：索引里已经有这个段的行、而且都带上文字，就直接标 `-OCRED` 不重读；行还没读完而切片目录仍在，就不标记、让下一轮的 `text` 继续；只有录制器再也够不着的那些行（切片已被回收，视频是唯一剩下的文字）才照旧抽帧。判断只看没有标记的文件名——`-INDEX`、`-ERRORn` 是本程序自己没跑完的活,照旧回滚重做。`windmaint backlog` 用同一个函数数给设置页看,所以按钮承诺的条数就是这一步真正会做的条数。
  Measured first, on a hand-started pass: `text` 23.9 min for 3799 rows, `convert` 7.6 min for 70 slices (one ffmpeg each), `refresh` 7 s, `expire` under 2 s, `reindex` still going after 40 min — nine steps, one thread, inside a 01:30–05:00 window. The reindex step was doing the work twice: the recorder writes a row per retained frame, the `text` step reads that row's masked copy, and once `convert` has cut the slice into a video the hour has already been read — yet reindex sampled the video every four seconds and OCR'd it again (about 14236 frames for 949 minutes of footage), after `rollback` had deleted the recorder's rows for that segment and replaced them with its own coarser ones. A census of the library that night says the same thing: rows from 09-25 to 09-28 are all `N_cropped.jpg` (reindex's shape), rows from 09-29 on are all timestamp-named recorder rows, and no segment held both. The rule now lives in one function, `wind_store::maintain::segment_coverage`, which both the step and the census ask: rows present and read → mark `-OCRED`, do not read again; rows still waiting and the slice folder still on disk → mark nothing, the next `text` step owns it; rows waiting with the slice swept → the video is the last copy of the words, so index it. Only unmarked filenames are asked, because `-INDEX` and `-ERRORn` are this program's own unfinished business and still roll back and redo.
- 六步在自己条目里并行，流水线本身不动：九步、顺序、名字、`step N/9` 与 `PROGRESS.MD` 全部照旧，`--limit`/`--dry-run`/`--manual`/停止请求/维护锁/`--idle-granted-by` 语义逐条保住,只是每项工作不再排队等前一项。`convert` 按切片开 ffmpeg（`Duty::Subprocess`,24 核机器上 4 路）;`previews` 按行解码缩放（`Duty::Decode`,8 路),SQLite 写入仍然每月一个事务、仍由主线程按行序提交;`refresh` 与 `backup` 按月文件并行(`Connection` 不是 `Sync`,一路一个连接),备份的清理留在所有复制之后;`reindex` 改跑多个 `wind-reindex` 进程,每个 `--shard K/N` 拿不相交的一段,各自起一个常驻 OCR 引擎——引擎启动约 30 秒,所以不足八段的图书馆只起一路。`wind-reindex --shard` 是新命令行列名,老写法一字未改。并行只在步内,所以进度条不需要新的形状。
  Six steps now work on several of their own items at once, while the pipeline itself is untouched: nine steps, same order, same names, same `step N/9` and `PROGRESS.MD`, and every `--limit` / `--dry-run` / `--manual` / stop-request / maintain-lock / `--idle-granted-by` meaning kept item for item. `convert` runs one ffmpeg per slice on `Duty::Subprocess` lanes (four on a 24-thread box), `previews` decodes and resizes per row on `Duty::Decode` (eight) while the index write stays one transaction per month issued by the calling thread in row order, `refresh` and `backup` go month by month (`Connection` is not `Sync`, so each lane opens its own) with the backup prune still after every copy has landed, and `reindex` walks the library in several `wind-reindex` processes, each dealt a disjoint `--shard K/N` of the sorted list and each carrying its own resident OCR engine — one engine start is about 30 s, so a library under eight segments still runs in a single lane. Parallelism stays inside each step, which is why the progress bar needed no new shape.
- 顺带修掉两处一直在白干活的地方：`text` 以前每读一行都把 `cache_screenshot` 整个 `read_dir` 一遍（3799 行 × 约 230 个目录），现在一轮一次；`refresh` 的存在性判定 `plan_existence` 是二次方的（每行在两张已经见过的名单里线性找），换成哈希表。还修了一条会咬人的边界：`wind-reindex` 把"库"认成了第一个目标文件的父目录,于是整库扫描的容纳检查拿最旧那个月份当根, newer 月份的每个文件都被判成 `outside the videos directory` 而跳过——两个月的图书馆永远索引不完第二个月。现在目录参数就是根,其下每个月份目录都在范围内,并留了一条测试守着。
  Two places that had been doing free work are fixed: `text` used to `read_dir` the whole `cache_screenshot` for every single row (3799 rows against ~230 directories) and now lists it once per run, and `plan_existence` was quadratic — a linear search of two "already seen" lists per row — and is now a hash lookup. A boundary that bites is fixed too: `wind-reindex` took the *first target's parent* as the library, so a whole-library walk used the oldest month folder as its containment root and every newer month's video came back `outside the videos directory`, skipped. An install with two months of footage could never finish indexing the second. The directory the caller names is now the root, with a test on each side of that rule.
- 并发写同一张月库不再一句 `SQLITE_BUSY` 就败：`Store::open`、只读连接和 `segment_coverage` 都加了 `busy_timeout`（30 秒）。分道跑时一个月边界段会碰同一扇门,以前第二家会直接吃一个 `-ERROR1`。
- 新增测试:`wind_base::pool` 的顺序/停止/重叠;`segment_coverage` 四种答案与"读不到就做"的兜底;`reindex` 的跳过与不标记、分道的不重不漏、`--shard` 两种拼写与拒绝、库根边界;`convert`/`previews`/`refresh`/`backup` 各自的多条目并行仍按条目序报告。`cargo test --workspace` 在本工作树里 4 项红,全是这份新检出的树没有 `bin\` 与 `userdata\`(CRLF 检出与缺产物),与本轮改动无关,主树另有它自己的环境红。

## 未发布 unreleased — 整理工作有了时间点，界面只由 winduiweb 承担 the backlog got a clock, and the HTML window is the only interface

- 修掉一个会自己长大的 bug：设置页里「整理遗留工作的开始时间 / 结束时间」两格填什么都存不住——按下保存、答一句"已写入"、重新打开还是旧钟点。原因在 `Settings::stage`：它写了十二个键，唯独漏了这两个，而 `load` 读它们、`Field::key` 命名它们、校验解析它们、控件编辑它们，所以没有任何一处会报错。补上这两行，并加了一道 `every_field_the_page_edits_survives_a_stage`：页面上每一行都必须带着一个与默认值不同的值走一遍"暂存—落盘—读回"，第十个字段再这样漏掉就会直接红。
  A bug that would have grown on its own is fixed: the two "organise the backlog from / until" boxes kept nothing — Save answered "written", and the next read printed the old hours. `Settings::stage` wrote twelve keys and omitted exactly those two, while `load` read them, `Field::key` named them, `validate` parsed them and the widget edited them, so nothing anywhere could notice. Both lines are there now, and `every_field_the_page_edits_survives_a_stage` walks every row the page offers through stage → disk → read-back with a value that differs from its default, so a tenth field cannot go missing the same quiet way.
- h265 / AV1 的录像不再是一句"你去装扩展"。窗口先问自己的播放器 `canPlayType`，答不行才把文件交给产品本来就在用的 ffmpeg，转出一份能放的副本存进 `cache\playback`，播的是副本、索引记的还是原来那段；副本按源文件的修改时间命名，重录一次自然作废，文件夹最多留二十份。装了 HEVC 扩展的机器一份都不转。转码失败时印的是 ffmpeg 自己那句话，不再是一个状态数字。
  An h265 or AV1 segment is no longer a sentence telling the user to go install a store extension. The window asks its own media element (`canPlayType`) first, and only when this machine truly cannot decode the codec does it hand the file to the `ffmpeg` the product already uses for every other piece of footage work, keep one playable copy in `cache\playback`, and play that — the index still names the original. Copies are keyed on the source's own mtime so a re-encode retires them, and the folder holds twenty at most. A machine with the extension never waits for a copy it did not need, and a failed transcode reports ffmpeg's own words rather than a status number.
- 「停止整理」现在真的会停，停止请求不再只在九个步骤边界被读到。`text`/`convert`/`refresh`/`expire`/`previews`/`backup` 每一步在自己的条目循环里问一次（一秒最多问一次，一旦看到就一路不再），`reindex` 改由父进程逐行读子进程的输出：行数报给进度条，并且能在约一秒内结束那个子进程——它自己只看整段边界。实测：在 `reindex` 里按下停止，0.5 秒收尾，发布 `stopped after 5 of 9 steps: stopped by request`。
  托盘同治：录制器现在把上一个 tick 干了什么写进 `cache\locks\RECORD_STATE.MD`，图标与菜单因此分得清 正在记录 / 未在记录：画面没有变化 / 没有在录制——这三件事过去被一把终身持有的锁说成同一件。设置页另多一个「数一数有多少要整理」：`windmaint backlog` 拿每一步自己的选择器在 dry-run 下数一遍，不拿锁、不写东西、不发请求，所以按下立刻整理之前先看得见有多少活。
  Stopping now stops. The request used to be read only at the nine step boundaries, so pressing it could mean waiting out a whole segment of OCR; every in-process loop asks inside its own items — at most once a second, and once seen the answer stays no — and `reindex` is read line by line by its parent, which feeds the row count to the bar and can end the child within a second. Measured: 0.5 s from the request to the pass publishing `stopped after 5 of 9 steps`. The tray is cured of the same conflation: the recorder publishes what its last tick did, so 正在记录, 画面没有变化 and 没有在录制 are three sentences instead of one lifelong lock.

- 「立刻整理」按下之后终于看得见干到哪了。维护轮现在把自己的状态发布到 `cache\locks\LOCK_MAINTAIN\PROGRESS.MD`：第几步（共九步）、这一步的名字、本轮是按钮起的还是钟点起的、已经跑了多少秒、这一步处理了多少项，结束时再写一句自己是完成、被叫停、失败还是没有交代。设置页那两个按钮下面因此多了一条九段进度条和一行说明，每两秒读一次这个文件；一个已经被杀掉的进程留下的 `running` 不会被相信，因为"在跑"要文件的说法和进程表的说法同时成立。`reindex` 在子进程里走库，那一步只有段数没有条数——没有为它编一个数字。
  `Organise now` finally shows what it is doing. The pass publishes its own state to `cache\locks\LOCK_MAINTAIN\PROGRESS.MD` — which of the nine steps is open, its name, whether the button or the clock started it, how long it has run, how many items that step has handled, and one closing sentence saying whether it finished, was called off, failed, or never said. Under the two buttons on the settings page sits a nine-segment bar and that sentence, re-read every two seconds; a `running` left behind by a killed process is not believed, because "running" now takes the file's own word *and* the process table's. `reindex` walks the library in a child process, so that step has a segment and no count — no number is invented for it.

- 移除 AI 端点的明文 HTTP 门禁：`open_ai_base_url` 写什么方案就按什么方案发，不再在 socket 之前把非回环的 `http://` 判成配置错误。挡住的是自建网关——同一局域网里跑着的 new-api/Ollama/LM Studio 全是 `http://192.168.x.x:port/v1` 这个写法，而拒绝的句子读起来像用户填错了。不新增开关，不加私有地址段判定；明文出网的代价由写下地址的人判断。三语文案与 `docs/adr/2026-09-28-the-ai-endpoint-scheme-is-the-users.md` 记下这笔账。
  The plain-HTTP gate on the AI endpoint is gone: whatever scheme `open_ai_base_url` carries is the scheme used, instead of `http://` off loopback being judged a configuration fault before a socket opens. What it blocked was self-hosting — a gateway on your own network is spelled `http://192.168.x.x:port/v1`, and the refusal read like the user's typo. No new switch, no private-range predicate; the cost of cleartext belongs to whoever typed the address. The three locales and `docs/adr/2026-09-28-the-ai-endpoint-scheme-is-the-users.md` carry the decision.
- 新增 `maintain_window_start` / `maintain_window_end`（例如 `03:30` 与 `05:00`）：遗留整理工作只在你说的这段时间里跑，
  过点自己停；跨零点算一次而不是两次。出厂为空，也就是保持原来的"空闲四十分钟"行为，不会因为升级把维护挪到别的钟点。
  New `maintain_window_start` / `maintain_window_end` (e.g. `03:30` and `05:00`): the deferred pass runs only inside the
  hours you name and stops when they close, with an overnight window counted as one appointment. Shipped unset, so an
  upgrade moves nobody's maintenance by itself.
- 录制器现在留住它启动的那一轮，因此不会重复起第二轮；`windmaint --manual` 表示这一轮是人按出来的，它受停止请求约束而
  不受窗口关闭约束。空闲开始时 OCR 引擎（连同约 21 MB 模型）被释放，需要时再自行启动；每三秒重读一遍设置文件的开销也去掉了。
  The recorder keeps the handle of the pass it started, so a second one cannot pile up; `windmaint --manual` marks a pass
  a person asked for, bounded by a stop request rather than by the window. The OCR engine is released when the screen
  goes idle (about 21 MB of models), and the settings files are no longer re-parsed every three seconds.
- `windui.exe`（egui 那个窗口）退出了发布集：托盘本来就只开 `winduiweb.exe`，现在发布包也不再暂存它，
  `windsvc doctor` 也不再把它算作安装完整性的一部分。它的 crate 仍然编译、仍然测——帧读取那道门、AI 提示词面板和
  表单字段声明只有这一份实现，web 窗口是同一份代码的第二个消费者。决定与理由记在
  `docs/adr/2026-09-27-winduiweb-is-the-only-interface.md`。
  `windui.exe` (the egui window) has left the shipped set: the tray already opened only `winduiweb.exe`, and a
  release no longer stages the other one, nor does `windsvc doctor` count it as part of an install's integrity. The
  crate is still built and still tested, because it is the only implementation of the frame door, the prompt panel
  and the form field declarations, and the web window is a second consumer of that same code. The decision and its
  reasoning are in `docs/adr/2026-09-27-winduiweb-is-the-only-interface.md`.

## v0.1.0 — 原生引擎 the native engine
> 2026-09-24

Windrecorder 现在是编译好的 Windows 程序。产品里没有 Python：压缩包里没有一个字节是，运行它的机器上也不需要。
Windrecorder is now compiled Windows binaries. There is no Python in the product: not in the zip, and not on the
machine that runs it. Unzip it anywhere and double-click `bin\Windrecorder.exe`.

- 解压到任意空目录，双击 `bin\Windrecorder.exe`：首次运行会自建 `userdata\`、从压缩包内的 `config_src\` 生成
  `userdata\config_user.json`、在托盘出现图标并开始记录。
  Unzip into an empty directory and double-click `bin\Windrecorder.exe`: the first run creates `userdata\`, seeds
  `userdata\config_user.json` from the `config_src\` that arrived in the zip, shows the tray icon and starts recording.
- 设了时间窗之后，录制那一遍里少了一次全文 1920 宽的预览编码：行先写下空的缩略图，窗口里的 `previews` 步骤照旧会把它
  补回来（它本来就按"存图够不够宽"挑活）。托盘也不再每秒重读两份设置文件——图标只问录制锁是不是活的，菜单打开时自会刷新。
  With a window named, the capture tick drops one full-width preview encode: the row is stored with an empty
  thumbnail and the window's own `previews` step refills it — that step already picks its work by "is the stored
  picture wide enough". The tray also stopped re-reading both settings files every second; the icon only asks
  whether the record lock is alive, and opening the menu refreshes for itself.
- 设了时间窗之后，录制那一遍也不再调用 OCR 引擎了：当下唯一不能事后补的是**加了遮罩的那一份**，所以录制时把
  `<时间>_cropped.jpg` 和原帧一起写进切片目录，行的文字留空；窗口里新增的 `text` 步骤（排在 `convert` 之前，因为后面的
  步骤会回收切片）读的就是这份遮罩副本，按时间从最早的行开始补，顺带把补出来才发现是重复的行折掉——只删索引行，画面文件留着，
  因为视频是从这些帧切的。补字时绝不回读原始 JPEG：那等于把用户要求不可检索的边缘检索了一遍。
  With a window named the tick no longer calls the OCR engine either. The one thing that cannot be recovered
  later is the **masked** copy, so the recorder writes `<stamp>_cropped.jpg` beside each frame and leaves the
  row's text empty; a new `text` step — ordered before `convert`, which is the step that recycles those slices —
  reads exactly that copy, oldest rows first, and folds away the rows that turn out to repeat once their text
  exists. Folding deletes the row and keeps the file: the footage is cut from those frames, and dropping one on
  a text judgement would be data loss. A row with no masked copy is reported and left waiting rather than read
  from the unmasked frame, which would index the very edges the mask exists to keep out.
- 设置页现在能看见这两个钟点（`HH:MM`，留空就是不按钟点安排），底下另有「立刻整理」和「停止整理」两个按钮：前者不等时间窗，
  马上开始；后者在下一个工作项收手，正在写的文件会先写完，不留半成品。
  The settings page now shows both clock times (`HH:MM`; empty means no schedule), with **Organise now** and
  **Stop organising** underneath: the first ignores the window and starts immediately, the second stops at the next
  work item — after the file currently open is finished, so nothing is left half-written.
- `bin\` 里现在有十一个可执行文件，`Windrecorder.exe` 是其中唯一以产品命名的一个，也是唯一需要你双击的一个：它找到
  `bin\windsvc.exe`（托盘）、把它启动、然后退到一边，命令行参数原样转交，所以 `Windrecorder.exe doctor` 打印的就是托盘的
  那份报告。它不加锁、不画菜单、不认识录制是怎么一回事；开机自启仍然指向 `windsvc.exe`，因为登录项要写的是那个持有锁的
  长驻进程。找不到托盘时它会说话：终端里是文字，没有终端时是对话框。
  `bin\` holds eleven executables and `Windrecorder.exe` is the only one named after the product, which makes it the one
  to click: it finds `bin\windsvc.exe` (the tray), starts it, steps aside, and hands over every argument it was given, so
  `Windrecorder.exe doctor` prints the tray's own report. It takes no lock, draws no menu and knows nothing about
  recording; sign-in autostart still names `windsvc.exe`, because the registry entry has to point at the long-lived owner
  of the lock. When there is no tray to start it says so — in the terminal, or in a dialog box when there is no terminal.
- 录制、索引、OCR、搜索、缩略图、维护、flag/note 书签与界面全部由 Rust 完成：`windrec.exe` 接管采集循环，
  `winduiweb.exe` 是搜索与设置窗口，`windsvc.exe` 是托盘。
  Recording, indexing, OCR, search, thumbnailing, maintenance, flag/note bookmarks and the whole interface are Rust:
  `windrec.exe` owns the capture loop, `winduiweb.exe` is the Search/Settings window, `windsvc.exe` is the tray.
- MCP 桥接是一个常驻 HTTP 服务（`windmcp.exe`），由托盘启停，因此机器上的每个 AI 助手共用一个进程而不是各自启动一个。
  The MCP bridge is a resident HTTP service (`windmcp.exe`) the tray starts and stops, so every AI assistant on the
  machine shares one process instead of spawning its own.
- AI 现在能把屏幕上的话写成段落，两层：每个录制片段一段，每一天一段，而且写它的有两方——本机 `windai.exe summarize` 用 AI 页填的
  接口写，机器外的助手通过 MCP 写，两边写的是同一批文件。桥给你的第一件东西不是写入端而是队列：`windrecorder_summaries_pending`
  说明哪天哪些片段还缺总结（或者总结已经跟不上画面、跟不上提示词了），并把每个片段的完整识别文字、窗口标题和时间一并带回来，所以
  拿材料只有一次调用。一天的总结在这一天每个片段都有站得住的总结之前会被拒绝写入，并逐个点名缺的是哪些；你明说 `allow_partial`
  它才写，并且把这一天真有残缺这件事写进文件里，以后每一次读都照实转述。东西放在 `userdata\result_ai_period_summary\` 和
  `userdata\result_ai_daily_summary\`，按产品日（默认凌晨 3 点起算）一天一个 JSON，用记事本就能打开看。没有任何长度上限，没有词表
  过滤，也没有默认裁剪。
  AI can now put the screen into prose, in two layers: one paragraph per recorded stretch and one per day, written by either side —
  this machine's `windai.exe summarize` through the endpoint on the AI page, or an assistant over MCP, into the same files. The first
  thing the bridge offers is not a writer but a queue: `windrecorder_summaries_pending` says which stretches of which day have no
  summary that stands (or one that no longer matches the screen, or no longer matches the prompt) and carries each stretch's whole
  recognised text, its window titles and its span, so gathering the material is one call. A day's summary is refused until every
  stretch of it has one — naming the missing ones — and is only written over a gap when the caller says `allow_partial`, in which case
  the gap is stored in the file and repeats itself on every later read. The text lives in `userdata\result_ai_period_summary\` and
  `userdata\result_ai_daily_summary\`, one JSON per product day (03:00 by default), openable in Notepad. No length cap, no word list,
  no clipped default anywhere on these paths.
- 发给模型的话全部摊开了：七份出厂模板在 `config_src\ai_prompts\`，你想改哪一份就在 `userdata\ai_prompts\` 放一个同名的纯文本文件，
  AI 页上七份都能直接编辑、并排看到出厂原文、一键恢复，还有一个“就拿这段真画面试这版词”的按钮——它真的会发一次请求，把发出去的原文
  和回来的答案一起给你看。`windai prompts` 和 `windmcp prompts-read` 是同一件事的终端入口，MCP 的队列也会把当前生效的提示词一并带出，
  所以外部助手写回来的段落会和本机写的读起来像同一个人。
  Every word sent to a model is now in the open: seven shipped templates in `config_src\ai_prompts\`, and to change one you drop a
  plain-text file of the same name in `userdata\ai_prompts\`. The AI page edits all seven, shows the shipped copy beside yours, restores
  a default in one click, and has a "try these words on a real stretch" button that really sends one request and shows you both the prompt
  as sent and the answer that came back. `windai prompts` and `windmcp prompts-read` are the same door in a terminal, and the MCP queue
  carries the prompt currently in force with it — so a paragraph written outside reads like one written inside.
- 抹掉一段历史的时候，总结跟着一起走：`windmaint forget` 清掉某些行的文字时，会把由这些行写出来的片段总结一并删掉，那一天如果已经
  无一物可依据，连当日总结一起删除（录像的过期清理 `windmaint expire` 只把它标成 `stale`，因为那是日历安排的删除，不是你在针对某段内容）；
  `--dry-run` 会先把这个代价算给你看。`windmaint backup` 现在也备份这两个目录，因为一年的人工智能人生总结是这一处唯一丢了就没有的东西。
  `windmcp doctor` 现在会说明这两个目录在哪、已经写了多少，以及这个桥能写字了。
  Erasing a period now takes its summaries with it: when `windmaint forget` blanks rows it drops the stretch paragraphs written from them,
  and a day left with nothing standing loses its own summary too (`windmaint expire` only flags a day `stale`, because that deletion was a
  schedule rather than a decision about content), and `--dry-run` prices that in advance. `windmaint backup` copies the two directories as
  well, because a year of AI-written life summaries is the one thing here that cannot be rebuilt. `windmcp doctor` says where the two
  directories are, how much they hold, and that the bridge can write.
- `windsetup.exe` 负责首次运行的目录结构、探测本机真正能跑的 OCR 引擎、并把旧安装的数据升级过来。它从不覆盖已有设置，
  升级前会把改动的文件逐个备份到 `userdata\backup\`。
  `windsetup.exe` lays out a first run, probes the OCR engines this machine can actually run, and migrates an older
  install's data forward. It never overwrites a settings file, and copies every file into `userdata\backup\` before
  changing it.
- 界面与托盘菜单的中英文案来自 `config_src\languages.json`，中文由系统字体绘制。
  The window's and the tray's copy come from `config_src\languages.json`, and Chinese is drawn with a system CJK face.
- 没有自动更新器。托盘里的“See what's new”打开的就是本文件，显示的版本号就是这个构建自带的版本号。
  There is no updater. "See what's new" opens this file, and the version in the tray menu is the one this build carries.

- 界面语言可以在设置窗口里选了，选项来自 `config_src\languages.json` 里真正翻译过的语言，每种语言用它自己的文字命名
  （简体中文 / English / 日本語）；保存后窗口立刻换语言，托盘菜单在下次打开时跟上。此前 `lang` 只能手工编辑
  `userdata\config_user.json` 再重启，等于没有这个功能。
  The interface language is now chosen in the settings window, from the locales the shipped catalog really translates,
  each named in its own script; the window relabels itself in the same click and the tray follows at its next menu
  opening. Before this, `lang` could only be changed by hand-editing `userdata\config_user.json` and restarting.
- OCR 引擎重新可选，而且真的会被执行：设置页的“本地 OCR 引擎”列出本机可运行的引擎，`windrec` 实时索引和
  `wind-reindex` 重读旧录像都按 `ocr_engine` 分派——图像路径进、文字出，也就是上游的贡献契约。此前内置引擎的路径被
  写死在两个二进制里，`ocr_engine` 没有任何读取者。原先依赖 Python 运行的 Paddle/RapidOCR、微信 OCR、ChineseOCR-lite
  在选择列表里标为“不可运行”而不是假装可选；任何以同样契约提供的 `.exe` 都能用 `ocr_engine_command` 登记后选用。
  The OCR engine is selectable again, and honoured: the settings page lists what this machine can drive, and both the
  live recorder and `wind-reindex` dispatch through `ocr_engine` on upstream's own contract (an image path in, text on
  stdout), which nothing used to read. The Python-hosted engines are listed as not driveable rather than offered as if
  they worked; an `.exe` of the same shape registers through `ocr_engine_command`.
- 点击结果旁的小图可以看到原图。索引里存的只是一枚小预览，两个窗口过去把它放大交差；现在按录制时的原始分辨率读取
  这一行自己的那一帧——优先是 `cache\i_frames` 里索引所记录的那张裁剪图，其次是 `cache\screenshot` 里该片段保留的
  截图，两者都已过期则用 ffmpeg 从视频中取该时刻的一帧，三者都没有时明确说明原因。HTML 窗口的详情面板同样如此。
  Clicking a result's picture opens the frame at the resolution it was recorded. The index stores only a small preview,
  and both windows used to enlarge it; the row's own frame is now read full-size — the OCR crop under `cache\i_frames`
  that the index names, else the slice's retained screenshot under `cache_screenshot`, else one frame taken out of the
  segment with ffmpeg — and a row with none of the three says so.
- HTML 窗口里点开一张图改成两步。以前按结果卡片上的图片，会同时打开这一行的详情栏和整窗原图，而读一次盘要 0.5–1 秒
  （从视频里取帧的那些行实测就是这个数）——很多下点击其实只是想看清这一条。现在单击图片只打开详情栏，画的是索引里那枚
  512 像素的预览，一点盘都不读；再点详情栏里的图片，才去读原始画面。原图查看器有了左右切换（← / → 或标题栏上的两个箭头），
  沿着刚才那份列表一条一条走：每一步只读一行，读过的行留在内存里，往回走 3 毫秒出图，不会再要一次磁盘。
  A click on a picture in the HTML window is now two clicks. Pressing a result card's picture used to open the row's
  column *and* the whole-window original together, which paid a 0.5–1 s disk read (measured, for the rows whose frame
  comes out of a video) on a press that often only means "let me look at this row". A click now opens the column and
  paints the index's own 512 px preview, reading nothing; the picture inside the column is the one that asks for the
  frame. The viewer gained `←`/`→` and two arrows in its title row, walking the list the row came from one row at a
  time — one row read per step, and a row already read stays in memory, so stepping back paints in 3 ms without
  asking the disk twice.
- 从录像里取出来的那一帧，现在留在 `cache\frame_snapshots\`。风记的录像就是由这些截图 JPEG 编码出来的，所以每一次点开
  原图，ffmpeg 都在重新推导程序自己曾经拥有的那张图——现在它只推导一次。文件名里带月文件、行号、这一行的录制时刻和取帧的
  门，所以索引重写让行号挪位时快照会落空重读，而不是把别的行的画面当成这一行。目录上限 256 MB，超了先删最旧的，刚写下的
  那张永远留着。截图那一门不复制：它本来就只是一次文件读取，复制下来只会让图片活得比 `vid_store_day` 还久。实测同一行：
  第一次 5.6 秒，重启窗口以后再开 57 毫秒。查看器换行时也不再从大图塌回小预览——上一张会压暗留在屏幕上，直到新的那张到。
  A frame pulled out of a video now stays in `cache\frame_snapshots\`. The recorder's video is encoded out of the
  JPEGs it captured, so every open of a row asked ffmpeg to re-derive a picture this program once held; it is derived
  once. The name carries the month file, the rowid, the row's own `videofile_time` and the door it came through, so a
  rewritten index that hands rowids out in another order misses and reads again rather than answering a row with
  another row's picture. The folder is held under 256 MB by dropping the oldest, never the one just written. The
  screenshot door is not copied: it already costs one file read, and a copy would keep a picture alive past the
  `vid_store_day` that swept its original. Measured on one row — 5.6 s the first time, 57 ms after the window is
  closed and started again. A step between rows also holds the previous frame, dimmed under the waiting line, instead
  of collapsing a 1080p picture back to the 512 px preview.
- 录像可以在窗口里播了，而且从这一行的那一秒开始。两个窗口过去只有“在资源管理器里定位”，而资源管理器不知道你点的是这 183 秒里的哪一秒。HTML 窗口用 WebView2 自带的 `<video>`：进度条、跳秒、倍速都是浏览器给的；egui 窗口自己拉 ffmpeg 的帧管道，一秒一张。两边都只交行键和裸文件名，文件由 Rust 侧在 `userdata\videos\{年-月}\` 里按目录列表查出来，网页和按钮都指不到别处的文件。实测：详情里写着 +8 秒的那一行，播出来就停在 8.00 秒，没人碰它，三秒后走到 11.01 秒，画面 1920×1166；同一段 2 363 552 字节的录像，`bytes=0-1023` 与 `bytes=2048-3071` 各回一个 206，首字节不同；`../config_src/languages.json` 和 `not-a-stamp.mp4` 一律 403。录像本身是 1 fps、无声、索引在文件头，所以第 S 秒就是第 S 帧，没有音画同步这回事。
  Footage plays inside the window now, opening on the second the row was indexed at. Both windows used to offer only *Locate*, and Explorer does not know which of a segment's 183 seconds you clicked for. The HTML window uses WebView2's own `<video>`, so the scrub bar, the second jumps and the rate menu are the browser's; the egui window drives an ffmpeg frame pipe, one picture per second. Both hand over a row key and a bare segment name and let the Rust side resolve the file inside `userdata\videos\{YYYY-MM}\` from a directory listing, so neither the webview nor a button can point at anything else. Measured: the row whose drawer reads +8 s opened at 8.00 s and reached 11.01 s untouched, at 1920x1166; `bytes=0-1023` and `bytes=2048-3071` of the same 2,363,552-byte segment each answered 206 with different first bytes; `../config_src/languages.json` and `not-a-stamp.mp4` were both refused 403. The segments are one frame per second, silent, and indexed at the head of the file, so second S is frame S and there is no audio to fall out of step.
  走的是这个窗口自己注册的 `windvideo` 协议，不是 Tauri 内置的 `asset:`：那个藏在 `protocol-asset` 后面，要 `http-range`，而这个离线仓库里没有它——在配置里打开它反而会让 `tauri-build` 报错，所以 Range 规则只在这里写一次，并带单元测试。egui 那一侧没有解码库可链（锁文件里只有 jpeg），帧因此来自 ffmpeg 进程，一秒一张，暂停就把进程杀掉：窗口收进托盘以后后台不留任何 ffmpeg。缺 ffmpeg 的机器上两个门都会说话，不会黑框——那也正是本来就没有 .mp4 可播的机器。
  The door is a scheme this crate registers itself, `windvideo`, not Tauri's built-in `asset:` handler: that one sits behind the `protocol-asset` feature, which needs `http-range`, and this offline workspace holds no copy of it — turning it on in the config fails the build instead of opening the door. So the range rule is written down once here, with tests. On the egui side there is no decoder to link against (the lock holds jpeg and nothing more), so the pictures come out of an ffmpeg process, one per second, and pausing kills it: nothing streams behind a window that has been hidden to the tray. A machine without ffmpeg is told so by both doors rather than handed a black box — which is also, on such a machine, a box with nothing to play.

- 关闭窗口不再等于退出程序：关闭按钮把窗口收进托盘（默认开启，可在设置里关掉），双击托盘图标或托盘的默认菜单项
  把它唤回。只有“用户要求后台”且“托盘仍在运行”时才隐藏，否则照常退出——没有托盘可唤回的隐藏等于把窗口弄丢。
  Closing the window no longer quits the app: the close button hides it to the tray (on by default, switchable in
  Settings), and the tray's default item or an icon double-click brings it back. It hides only when the user asked for
  a background mode and a tray is alive to raise it again.
- 新增“登录时自动启动”：按当前用户写入 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`，指向本安装的
  `bin\windsvc.exe`（托盘，而不是窗口），不需要管理员权限。旧版靠 Startup 文件夹里的快捷方式，重写的过程中“创建”
  这一半丢失了，只留下删除；这里以注册表补回。设置保存后立刻生效并回报结果，写不进去会讲清楚。
  "Start on sign-in" registers the tray — never a window — for the current user under
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, with no administrator rights. Upstream did this with a
  Startup-folder shortcut whose *creation* half was lost in the rewrite, leaving only the deletion; the registry puts
  it back. The setting applies on Save and the window reports what the registry said.
- 设置页从 8 个键增加到 12 个，并且每个字段的标题与说明都改走 `languages.json`：换了语言的设置页不再是英文。
  The Settings page went from 8 keys to 12, and every field's label and help now resolve through `languages.json`, so
  a window in another language no longer shows an English settings page.
- 预览图不再只是一枚邮票：默认宽度定为 512 px（上游的 70 px 是一枚邮票，240 px 仍比 HTML 窗口画的结果卡片窄，
  放大就是糊），质量 70，并且新增 `windmaint previews` 把**已经录下的**每一行按其原始帧重绘到新宽度——只重画图，
  不重跑 OCR，所以文字内容一字不改。该步骤已进入空闲维护的流水线，也可以自己跑一遍先看 `--dry-run` 的清单。
  设置页在这一行下面直接说明本机存的预览比卡片画的窄，以及重绘要等空闲维护这一趟——数字改了却看不见变化，
  是这个产品已经错过两次的死控件形状。
  Previews stopped being stamps: the shipped width is 512 px — upstream's 70 px is a stamp, and even 240 px was narrower
  than the result card the HTML window paints, so the picture was being *stretched* to fill it — at quality 70, and
  `windmaint previews` redraws the rows a user **already has** from their own frames: pictures only, never a re-OCR, so
  no row's text changes. The step is in the idle pipeline and can be run by hand, `--dry-run` first if you like. The
  settings row now says out loud when this install stores previews narrower than the box they are drawn in, and that the
  rows already on disk wait for that pass — a number that appears to change nothing is the dead-control shape this
  product has now been corrected for twice.
- 搜索结果、一天的时间带、整月灯箱三处的图都能点开看大图：点开的是这一行按录制分辨率保存的那一帧，取图的钥匙是行的
  身份（月文件 + rowid），原图的路径由索引说了算，而不是由界面递过来的路径决定该读哪个文件。
  A click opens the recorded frame in all three places a picture is drawn — the result cards, the day's strip, and the
  month's lightbox — and the door is keyed by the row's identity (month file plus `rowid`) rather than by a path the
  interface hands over: the index is what says which screenshot or which segment to read.
- 一天页面里三处看起来是空的标签都有了内容：活动时间图每一根柱子下面原本什么都没有，因为它从一段已经是 `HH:MM`
  的文字里切第 11–13 个字符；“时间都去了哪”按整分钟四舍五入，只用了四十秒的窗口一律显示 `0m`，在屏幕上跟面板没填上
  一模一样；时间带和整月灯箱自己把索引里的秒数当时间戳格式化，而那些秒是“本地墙上时间”而不是时刻，于是它们比同一行
  结果卡片上的时间差了一整个时区（本机是 8 小时）。现在时刻由读数据那一侧格式化好送过来（`StripCell.clock`、
  `LightboxTile.stamp`），时长说成 `22s` / `3:05` / `1:02:03`。
  Three labels on the day screen that looked empty are filled in. Every bar of the activity chart printed nothing at
  all, because it sliced characters 11–13 out of a string that was already `HH:MM`; "where the time went" rounded to
  whole minutes, so a window you used for forty seconds read `0m`, which on screen is indistinguishable from a panel
  that never filled in; and the strip and the month's lightbox phrased their own timestamps out of raw seconds — which
  in this index are naive-local, not an instant — so they sat a whole timezone away from the same row's result card.
  Instants now arrive already phrased by the side that read the row (`StripCell.clock`, `LightboxTile.stamp`), and a
  duration says `22s`, `3:05`, `1:02:03`.
- 录制页与 AI 页整页跟随所选语言：字段标题、分组标题和每行下方的说明文字都改成从 `languages.json` 取值。标题复用上
  游已有的文案行（例如 `rs_text_record_mode`），说明文字则重写成面向使用者的一句话——原来那里印的是给维护者看的工
  程注释（`wind-reindex`、moov atom 之类），而且只有英文。AI 页密钥状态那行提示同样补上了语言条目。
  The Recording and AI pages follow the chosen language end to end: field labels, section headings *and* the line of help
  under each row now resolve through `languages.json`. The labels reuse upstream's own rows (`rs_text_record_mode` and
  friends); the help lines were rewritten as one sentence for whoever is configuring the number, because what sat there
  was maintainer prose about `wind-reindex` and moov atoms — in English only. The AI page's stored-key pill got the
  same treatment.
- 中文 README 的“如何使用”一节此前仍在教人运行 `start_app.bat`、删除 `.venv`、再跑 `install_update.bat`——那些文件已
  随 Python 应用一起删除。现在这一节写的是这台机器上真实存在的东西：双击 `bin\windsvc.exe`、两个窗口、MCP 开关在
  AI 页、开机启动用 `windsetup autostart --enable`；英文 README 同步补上 MCP 与开机启动两段。
  The Chinese README's "如何使用" was still telling people to run `start_app.bat`, delete `.venv` and re-run
  `install_update.bat` — files that left with the Python application. The section now describes what is on the machine:
  double-click `bin\windsvc.exe`, the two windows, the MCP switch on the AI page, and `windsetup autostart --enable` for
  sign-in start. The English README gained the MCP and sign-in paragraphs to match.
- MCP 桥接有了正门上的开关：AI 页新增 5 个键（`enable_mcp_server` 与它绑定使用的地址、端口、是否需要令牌、令牌
  本身），托盘在每次打开菜单时重读设置并据此启停桥接，不再需要重启托盘才能生效。地址与令牌的取舍仍由 `windmcp`
  自己把关：非本机地址而不要求令牌会被拒绝启动。
  The MCP bridge got a door on the front of the house: the AI page now carries its five keys — `enable_mcp_server`, the
  address and port it binds, and whether a bearer token is required — and the tray re-reads the settings each time its
  menu opens, starting and stopping the bridge to match instead of needing a restart. `windmcp` still keeps its own
  veto: an off-machine address with authentication off is refused at startup.
- 微信 OCR 现在能被原生引擎驱动了。上游那一版靠一个 Python 包去起 `WeChatOCR.exe`、通过 mmmojo 命名管道
  收发 protobuf 结果；这条通道本身不需要 Python，所以 `wind_base::wxocr` 用同样的方式把它接了回来：一个常驻
  子进程、一次加载 21 MB 模型，之后每帧只有一次请求和一次回调。设置页的“本地 OCR 引擎”里它现在是一行可选
  项，`windsetup check-engines` 会像测其他引擎一样给它打分——本机实测三张基准图 97.4% / 94.0% / 97.3%
  （内置 Windows OCR 是 92.7% / 无语言包 / 90.2%），单帧约 0.3–0.5 秒。二进制不在这个仓库里，也不在压缩包里：
  它来自第三方仓库 `kanadeblisst00/wechat_ocr` 的 `bin\` 目录，约 63 MB，该项目自己注明仅供学习与个人使用、不得商用；把 `ocr_lib\wxocr-binary\` 放好，
  这一行就会从“缺少文件”变成可选，缺哪个文件也会直接说出来。
  WeChat's OCR is drivable by the native engine now. Upstream reached it through a Python package that
  started `WeChatOCR.exe` and exchanged protobuf frames with it over an mmmojo named pipe; the channel needs
  no interpreter, so `wind_base::wxocr` speaks it directly — one resident child, ~21 MB of models loaded
  once, then one request and one callback per frame. It is a selectable row in the engine picker, and
  `windsetup check-engines` scores it like every other engine: 97.4% / 94.0% / 97.3% on the three shipped
  fixtures here, against the built-in Windows engine's 92.7% / no language pack / 90.2%, at 0.3–0.5 s a
  frame. The binaries are not in this repository and not in the zip — they are a third-party extraction of
  WeChat's own component — the `bin\` folder of
  [`kanadeblisst00/wechat_ocr`](https://github.com/kanadeblisst00/wechat_ocr), about 63 MB, whose own
  README says study and personal use rather than commercial. Put
  `ocr_lib\wxocr-binary\` in place and the row becomes selectable; leave a file out and the row says which.
  它的超时是一分钟，所以“安静地死掉”是最坏的一种坏法：连续三帧没有回答，录制端就不再每帧去问，改成每 40 帧试一次
  （帧照样保存、照样按窗口标题入档）；回扫端直接中止这一条并在报告里说明原因，后面的视频不再尝试。命令行引擎不走
  这条路——它们失败得很快，报告里的 unreadable 计数就是它们的样子。
  A silent one is the worst way for it to break, because its wait is a minute: after three frames in a row with no
  answer the recorder stops offering every frame and retries once every fortieth — the frame is still written and
  still indexed on its window title — while a back-index abandons the segment, names the reason, and leaves the
  rest of the library alone. Command-line engines keep the old shape, since failing fast is what makes the
  unreadable count in the report enough.
- 两个额外组件现在有自己的下载，并且缺 ffmpeg 这件事第一次有了会被说出来的地方。发布包里没有微信 OCR
  组件、也没有 ffmpeg，这两件都不会改变：前者是第三方从微信里提取出来的腾讯代码和模型、自带“仅供学习与
  个人使用”的限制，后者是别人 GPL v3 的构建，都不该塞进一个 GPL-2.0 的应用包里。但“不该塞进来”不等于
  “让用户自己拼”：`windcap\extras.ps1` 把机器上已有的文件打成
  `Windrecorder-wxocr-binary-<version>.zip` 与 `Windrecorder-ffmpeg-<version>.zip`，各自带 `.sha256`、包内带
  `MANIFEST.sha256` 和一份说明，解压的目标就是应用目录——放进去的位置正是应用第一个去找的位置。打完它会把
  包再解压一次，逐个文件对回清单，然后真的跑：微信那份必须给三张基准图打出分（97.4% / 94.0% / 97.3%），
  ffmpeg 那份必须被 `windsetup doctor` 认成来自*这个*安装；任何一条不成立就报出来、非零退出，而不是安静地
  产出一个不能用的包。它自己从不下载任何东西——把“去抓第三方的二进制”写成构建的一个副作用，是比缺文件
  更糟的设计。
  顺带补上的是那句一直没人说的实话：`windsetup doctor` 现在有一节 `VIDEO STEP`。没有 ffmpeg 的时候什么都
  不会失败——截图照拍、照堆在 `cache_screenshot\`，只是永远不会有视频——所以症状看起来像“没在录”，而实际
  上是“录了但编不出来”。这一行现在把解析到的路径、它是来自安装目录还是 `PATH`、以及什么都没有时的后果
  一起说出来，`--json` 里也有同样一份。
  Two extra components now have their own download, and a missing ffmpeg has somewhere to be said out loud.
  The payload carries neither the WeChat OCR component nor ffmpeg, and that does not change: the first is
  Tencent's code and models extracted by a third party under a study-and-personal-use limit, the second is
  somebody else's GPL v3 build, and neither belongs inside a GPL-2.0 application zip. But "does not belong
  inside" is not the same as "assemble it yourself": `windcap\extras.ps1` packs what is already on the
  machine into `Windrecorder-wxocr-binary-<version>.zip` and `Windrecorder-ffmpeg-<version>.zip`, each with a
  `.sha256` beside it and a `MANIFEST.sha256` plus its own instructions inside, and each unpacks into the
  app folder — which is the directory the application looks in first. Then it unpacks the archive it just
  made, re-hashes every file against the manifest it just wrote, and runs it: the WeChat one has to score
  the three benchmark pictures (97.4% / 94.0% / 97.3%) and the ffmpeg one has to be resolved by
  `windsetup doctor` as coming from *this* install. Either failing is reported and exits non-zero rather
  than producing a file nobody should have to trust. The script never downloads anything itself — making
  "fetch a third party's binaries" a side effect of a build is a worse design than a missing file.
  What came with it is the sentence nobody used to say: `windsetup doctor` has a `VIDEO STEP` now. With no
  ffmpeg nothing fails — screenshots keep being taken and keep piling up in `cache_screenshot\`, they just
  never become a video — so the symptom reads as "it is not recording" when the truth is "it recorded and
  cannot finish". The line names the path it resolved, whether that is the install's own copy or somebody
  else's on `PATH`, and what the absence costs; `--json` carries the same facts.
- 隐私遮罩终于有了门。`ocr_image_crop_URBL` 决定屏幕的哪几条边永远不进可搜索索引——任务栏、时钟、银行余额——
  录制端和回扫端一直认真执行它，两个窗口里却没有任何一行能改它，只能手编 JSON；上游是有控件的
  （`ui/setting.py`：每台显示器上/右/下/左四个 0–40 的数字输入）。现在设置页每台屏幕一行四个框，上限就是
  上游那个 40，旁边写着这组百分比在该屏真正遮掉的像素数——那个数字来自录制器调用的同一个函数，所以屏幕上
  看到的和索引里缺的不会是两套答案。列表没覆盖到的屏幕取 6/6/6/3，正是画框器对越界槽位本来就用的兜底，
  于是保存之后文件第一次写出了实际发生的事。
  同一轮清掉了两个假开关。录制页的「截屏相似度」调的是 `compare_image_similarity_np`，而原生门控早已换成
  dHash 距离（`core/src/gate.rs` 开头就写着这次替换），那一行的帮助文字其实自己承认「改了也不会影响原生
  录制」——所以删掉这一行；键留在文件里，`Config::save` 写的是合并表，Python 版仍然认它，并且有测试钉住
  「删控件不等于删用户的设置」。`record_mode` 那行留下：它只能选到 `screenshot_array`，而那正是引擎做的事。
  「高亮命中的词」以前什么都不做——两个窗口的上色都取自后端返回的同一个词表，于是让那个开关去决定词表是否为空，
  一处改动两扇门上色同时生效，并有测试保证「答案不变、只有颜色变」。
  The privacy mask finally has a door. `ocr_image_crop_URBL` decides which edges of the screen never reach the
  searchable index — the taskbar, the clock, the balance in a banking page — and the recorder and the back-index
  have honoured it all along while neither window offered a single row to change it, leaving JSON as the only
  editor; upstream had one (four 0–40 inputs per display in `ui/setting.py`). The Settings page now shows one
  line of four boxes per screen, under upstream's own ceiling of 40, beside the number of pixels that group
  actually blackens on that panel — a figure taken from the same function the recorder calls, so what the row
  shows and what the index lacks cannot be two different answers. A screen the list does not reach keeps
  6/6/6/3, which is the fallback the painter applies to an out-of-range slot anyway, so the file says what has
  been true all along.
  Two switches stopped being decorative in the same pass. The Recording page's screenshot-similarity row tuned
  `compare_image_similarity_np`, and the native gate replaced that metric with a dHash distance — `core/src/gate.rs`
  opens by saying so — while the row's own help admitted the value retunes nothing a native recording does. The
  row is gone; the key stays in the file, because `Config::save` writes a merged map and the Python app still
  honours it, and a test pins that deleting a control is not deleting somebody's setting. `record_mode` stays,
  because the only mode it can name is the one the engine runs. The highlight checkbox did nothing at all: both
  windows colour from the one term list the backend returns, so the switch now decides whether that list is
  empty — one change, both doors, with a test that the matched rows are identical and only the colouring moves.
- 登录启动项现在可以直接问：`windsetup autostart` 打印 `HKCU\...\Run\Windrecorder` 当前的内容与它指向哪个程序，
  `--enable` / `--disable` 修改它，`--dry-run` 先看会做什么。设置页的勾选框走的是同一个函数，两边不可能各说各话。
  The sign-in entry can simply be asked: `windsetup autostart` prints what `HKCU\...\Run\Windrecorder` currently says
  and which program it points at, `--enable`/`--disable` change it, and `--dry-run` says what it would do first. The
  settings page's checkbox calls the same function, so the two cannot tell different stories.
- 遮罩只管"以后"、不管"已经"，这个洞现在补上了：`windmaint forget --day 2026-09-22`（或 `--from/--to`，再加
  `--keyword 收入`）把某个时段已经入库的 OCR 文字、窗口标题、浏览器链接和预览图一起置空，行本身和视频文件都留着。
  日期用与 `windcapctl query` 同一个解析器，`--dry-run` 报的是同一条 SQL 谓词数出来的行数；不给时段的 forget 直接拒绝，
  因为漏写一个 flag 不该等于清空整个库。报告末尾会列出这条命令够不着的东西——录像像素、`cache\db_backup\` 里
  仍带旧文字的几份月度备份、以及 `result_ai_extract_tag\` 里的 AI 结果。
  The mask only guarded the future; there was no door on what had already been indexed. `windmaint forget --day
  2026-09-22` (or `--from`/`--to`, optionally `--keyword revenue`) now blanks the stored text, window title, link and
  preview for a period you name, leaving the rows and the footage itself intact. The dates go through the same parser
  `windcapctl query` uses, `--dry-run` counts with the very predicate the write then uses, and a `forget` with no
  period is refused outright — a forgotten flag must not mean "erase the library". Its report names what the command
  cannot reach: the pixels in the video, the month-file copies under `cache\db_backup\` that still hold the old
  text, and the AI caches under `result_ai_extract_tag\`.
- 重读旧录像时不再拿默认值换掉你的遮罩：多屏机器上，只要一段录像的画面尺寸和当前桌面并集对不上（拔掉过一台显示器、
  或两屏机器只录其中一屏，都是这种情况），`wind-reindex` 之前会放弃你设置的百分比、改用内置的 6/6/6/3——于是设了 40%
  的人，旧素材只有 6% 被盖住。现在这条分支取你所有屏里最宽的那一档，任何情况下都不会比你写的更少；只有配置真的为空
  时才落到内置默认。`windrec doctor` 也会打印 `ocr mask`，把每个边的百分比换算成实际行、列。
  Re-reading old footage no longer trades your mask for the built-in default. On a machine with more than one panel,
  any segment whose frame size differs from the desktop union — a monitor since unplugged, or a two-panel machine
  that records one — made `wind-reindex` discard the configured percentages and paint 6/6/6/3 instead, so a 40 %
  setting hid 6 % of the old library. That branch now takes the widest band any panel was given, which can never hide
  less than you asked for, and the shipped default applies only when nothing is configured. `windrec doctor` prints
  `ocr mask` with every edge converted into rows and columns.
- `windsetup init --root .` 不再拒绝它自己所在的目录：`.` 被路径校验按字面比较，`.\userdata` 于是"解析到了安装根目录
  之外"，而 doctor/check-engines/migrate 对同一个 `.` 一直照收。现在 `--root` 在执行任何一步之前先变成绝对路径。
  `windsetup init --root .` no longer rejects the directory it was typed in. `.` was compared lexically, so
  `.\userdata` "resolved outside the install root" while `doctor`, `check-engines` and `migrate` had accepted the same
  argument all along; `--root` is now made absolute before any step runs.
- 闲时维护的删除报告不再被丢掉：录制器以前用 `Stdio::null` 启动 `windmaint all`，而这趟是唯一会真的删文件、重压缩的
  进程，它也不是托盘的子进程，所以那些"N 段过期、M 个文件移除"的行从来没人看得到。现在两个流都追加到
  `cache\logs\windmaint-idle.log`，启动那一行也会说出日志在哪。
  The idle maintenance pass keeps its own report. The recorder used to spawn `windmaint all` with stdout and stderr
  sent to null, and that pass is the only thing that really deletes or re-encodes — nor is it a supervised child, so
  the tray's log rotation never saw it. Both streams now append to `cache\logs\windmaint-idle.log`, and the launch
  line names the file.
- 回收站会过期了：`recycle_deleted_files` 默认开启，过期录像被搬进 `userdata\trash\<运行时间戳>\` 而不是删除，而全
  项目没有任何一处再碰那个目录——默认配置下"清理"释放的字节数是 0，磁盘只是换了个位置继续涨。`windmaint expire` 现在
  顺手清掉 7 天前的回收批次，只认自己那种完整时间戳命名的目录，别的名字一概不动，并按名字而不是文件系统时间判断新旧；
  `windmaint doctor` 的 retention 那一行会报出那个目录里现在占了多少。
  The trash now ages out. `recycle_deleted_files` defaults to on, expired videos were moved into
  `userdata\trash\<run stamp>\` rather than deleted, and nothing in the workspace ever looked at that folder again —
  so retention reclaimed exactly zero bytes on a fresh install while the disk kept growing. `windmaint expire` now
  prunes runs older than seven days, recognising only folders named by one of our own full run stamps and judging age
  from the name rather than the filesystem; anything else in there is left alone, and `windmaint doctor`'s retention
  line reports what the folder is holding.
- MCP 的 `timestamp` 参数声明修好了：工具返回的是 JSON 整数，输入 schema 却只写 `"type": "string"`，于是严格按
  schema 校验的主机会拒绝把自己刚拿到的值传回去——正是说明文字让它做的那件事。服务端本来就两种都收，现在声明也是。
  The MCP bridge's `timestamp` argument now says what it takes. Every payload returns a JSON integer while the input
  schema declared `"type": "string"`, so a host that validated an argument before sending refused the value it had just
  been handed — which is the one thing the description tells it to pass back unchanged. The server accepted both
  already; now the schema does too.
- HTML 窗口的开关在窄窗口里会被压扁：设置行的开关按钮声明 44 px 宽，实际只拿到 17 px——它是 flex 项又没有行内内容，
  最小内容宽度为 0，标签那边的 `min-w-0` 保护不到它——于是圆形滑块滑到了药丸外面。加 `shrink-0` 修好。顺带说明：
  之前"设置页右栏被裁切"的报告是假的，那是截图边界（本机四屏、并集 5920x2880，窗口比截到的画面更宽）；这次改用
  WebView2 自带的 CDP 端口量真实 DOM，2560 / 1536 / 900 / 720 四档下 `docScrollWidth` 都等于视口宽、没有任何元素越过视口右缘。
  The HTML window's switches were being squeezed on a narrow window: the settings row's toggle declares 44 px and was
  given 17 — a flex item with no in-flow content has min-content 0, so the label's `min-w-0` never covered it — and the
  knob slid outside its pill. `shrink-0` fixes it. And the earlier "the settings page clips its right column" report was
  false: that was the *screenshot's* edge (four panels here, a 5920x2880 union, so a maximised window is wider than any
  capture of it). Measured through the window's own CDP port instead, `docScrollWidth` equals the viewport at 2560,
  1536, 900 and 720, with nothing past the right edge at any of them.
- README 里两句会被陌生人原样复述的话已经改准：一是"所有能力完全本地、不联网、不上传"——开启 AI 标签后并不成立，
  现在写明只有你亲手打开、亲手填地址的 AI 标签与 MCP 桥会向外发；二是"把浏览器链接写入数据库"——原生录制器没有
  UIAutomation 读取器，`deep_linking` 每行为空（这一点录制页和启动时的提示本来就说），现在 README 也照实说，并把
  "闲时维护"具体列成它真正做的几件事。
  Two README sentences a stranger would repeat and be wrong are now accurate. "All its capabilities run completely
  locally, without an Internet connection or uploading any data" stopped being true the moment AI tagging is switched
  on, so the sentence names the two features that reach out and that both ship off. "Update the OCR text, page title,
  browser url … to the database" was never true of the native recorder — `deep_linking` is empty by design until a
  UIAutomation reader exists, which the Recording page and the recorder's own startup note already say; the feature
  bullet now does too, and "idle maintenance" is spelled out as the five things that pass really does.
- 托盘不再在“点开菜单”的那一次右键上消失。原因在托盘自己的消息循环里：`TrackPopupMenuEx` 的模态循环会继续派发
  本线程的存活定时消息，而托盘把它的宿主状态借给了这一次调用，于是 1 秒后到的那一拍二次进入同一个借用——`RefCell`
  对此的回答是 panic，而 panic 无法穿过窗口过程的边界，只剩 abort：没有控制台、没有对话框、没有一行日志，屏幕上
  只看见图标没了。现在菜单在借用之外弹出，循环里到达的定时消息一律拒答（图标在菜单关掉后的下一拍补上），嵌套借用
  也不再可能成立。同一支脚本打两边：交付件在第 1 轮右键、菜单被真实跟踪 3.3 秒后退出（事件日志 `0xc0000409`，
  偏移 `0xfc279`），修好的那份 3 轮、15.0 秒全程跟踪之后还在。脚本留在 `__verify__\prove-tray-rightclick.ps1`，
  它会记下菜单究竟开着多久——一个从未弹出的菜单会让“没崩”变成假消息，这个检查的第一版差点就那样交了。
  The tray no longer dies on the click that opens its menu. `TrackPopupMenuEx` runs a modal loop of its own and
  keeps dispatching this thread's liveness timer inside it, while the tray handed that call a borrow of its own
  state — so the tick a second later re-entered the same borrow, a `RefCell` answers that with a panic, a panic
  cannot unwind through a window procedure, and what is left is an abort in a binary with no console, no dialog
  and no log line: an icon that simply vanished. The menu is now popped outside the borrow, a tick arriving
  inside the loop is refused (the icon catches up on the next one), and a nested borrow can no longer be taken.
  One script, both builds: the delivered binary exited in the first round after 3.3 s of genuinely tracked menu
  (event log `0xc0000409`, offset `0xfc279`), the fixed one took 3 rounds and 15.0 s of tracking and stayed
  alive. `__verify__\prove-tray-rightclick.ps1` reports how long the menu was really open, because a click that
  never opened one makes "it did not crash" worthless — the first version of this check nearly shipped that.
- 界面读的是它自己的旧快照，而“旧”的原因是一句问错的话。每个窗口都不直接查在用索引，只查月度数据库旁边的
  `_TEMP_READ.db` 副本，副本唯一该停止更新的时候，是闲时维护真的正在改写那个月度文件——读到一半的数据库比旧一点的
  数据库更糟。问题是 `windmaint` 收尾只删自己的 `PID`、只 rmdir 自己创建的目录，所以一次回收过死锁的运行、一个被托盘
  清空的 Python 容器，都会让 `cache\locks\LOCK_MAINTAIN` 以“空目录”的样子永久留在盘上；而四个读路径
  （`windui`/`winduiweb` 的 backend、`windcapctl`、`windai`、`windmcp`）当时问的都是“这个目录在不在”。于是维护跑过一次
  之后，所有窗口永久停在那之前的副本上：本机实测副本停在 9 行、最后一条 09-22 19:51:12，而当天索引里已经有 21 行。
  现在四个读路径问的是同一句话 `Config::maintain_lock_claimed()`——目录里的 `PID` 是否指向一个还活着的进程——与两处
  doctor 早就写下的判据一致（空容器是无主的，不是认领）；维护真在跑时照旧让路。两个回归测试各钉一边：留下的空容器必须
  让副本刷新、把当天的行读出来；活着的主人必须让副本不动。同一支脚本打两边：`__verify__\prove-stale-read-snapshot.ps1`
  用两个一次性安装根（一个留着空容器、一个什么都不留）跑同一个 `stats`，改之前的二进制在第 3600 秒就不动副本了，改之后
  两边都把副本重建。README 里那条“打开窗口没有近期数据”的问答也跟着改成了实际做法，它原本给的是“删掉副本再等”。
  The window was reading its own old snapshot, and the reason was a wrongly-worded question. No reader opens a live
  index — each one works on the `_TEMP_READ.db` copy beside the month file, and the only time that copy is allowed to
  stop being rebuilt is while the idle maintenance pass is genuinely rewriting the file, because a half-copied database
  is worse than a slightly old one. But `windmaint` removes only its own `PID`, and rmdir's only a directory it created
  itself, so a run that reclaimed a corpse — or a Python container the tray had swept empty — leaves
  `cache\locks\LOCK_MAINTAIN` standing on disk forever with nothing in it. All four readers (`windui`/`winduiweb`'s
  backend, `windcapctl`, `windai`, `windmcp`) were asking whether that directory *exists*, so after one idle pass every
  window froze on the snapshot from before it: measured here, the copy held 9 rows with its newest at 2026-09-22
  19:51:12 while the index already held 21 rows for the day. The four now ask one shared question,
  `Config::maintain_lock_claimed()` — does the `PID` inside name a process that is still alive — which is the
  distinction both doctor reports had already been printing in words; a live owner still defers the refresh. One
  regression test on each side: a container left behind must refresh the copy and return the day's rows, a live owner
  must leave the copy alone. One script, both builds: `__verify__\prove-stale-read-snapshot.ps1` runs the same `stats`
  against two throwaway roots, one holding the empty container and one holding nothing at all, and the binary from
  before this change stops rebuilding the copy once the two are 3600 s apart where the fixed one rebuilds in both. The
  README's "there is no data in the recent period" answer describes what happens now
  instead of telling you to delete the copy and wait.
- 一天的“活动时间”终于和“在电脑前多久”是同一件事。它原先按柱子的宽度封顶：相邻两帧之间超过 6 分钟的那部分一律不计，而那
  6 分钟是 `BUCKET_SECS`——它存在的理由只有一句“一天画 240 根柱子而不是 1440 根”。屏幕静止超过 5 分钟录制器本来就会暂停
  （`screentime_not_change_to_pause_record`），于是每一次读文档、看讲座、想问题都正好被削成 6 分钟。现在封顶的尺子是
  `Config::presence_gap_secs()`：取录制器自己的节段长度 `record_seconds` 与静止暂停门槛的较大者（本机 900 秒），两个都是
  录制页上看得见、改得动的设置，而画图的分辨率从此只管画图。超过一节段的时间按一节段计，不再整段丢掉——索引只能说“这段
  没有新东西”，不能说“这段时间没有人”。同一把尺子现在也管整月散点：那里原来是 `最后一帧 − 第一帧`，一点都不封，
  所以散点上的一个点可以画到 5.03 小时高，点开那一天却写着 0.25 小时。`windcapctl day` 多印一行 `presence gap`，
  把这台机器真正用的那个数说出来。
  A day's "hours on screen" now measures the thing it names. It used to be cut at the width of a bar: any stretch between
  two captures past 6 minutes counted as nothing, and that 6 was `BUCKET_SECS`, whose only justification is that a day
  draws 240 columns instead of 1440. The recorder pauses once a screen has been still for 5 minutes
  (`screentime_not_change_to_pause_record`), so every read paper, lecture and moment of thought was trimmed to exactly 6
  minutes. The ruler is now `Config::presence_gap_secs()` — the greater of the recorder's own `record_seconds` and its
  still-screen pause threshold (900 s on this machine), both settings visible and editable on the Recording page — and
  the chart's resolution decides nothing but the chart's resolution. A stretch past one segment is counted as one segment
  rather than dropped: the index can say nothing new was captured, and it cannot say nobody was there. The same ruler now
  draws the month's scatter, which had been `last − first` with no cut at all, which is why a day could stand 5.03 hours
  tall in the month and read 0.25 hours when opened. `windcapctl day` prints the number this install actually uses, as
  `presence gap`.
- 修好了一件会让“今天”凭空消失的事：屏幕静止超过 5 分钟之后，录制器**再也醒不过来**。暂停分支只关段、跑维护、睡 10 秒，
  而“该不该继续暂停”唯一的判据在抓帧之后——段一关完（`is_committable()` 此后恒为 false），就没有任何代码路径能把
  `paused` 清掉。日志里那句 “Ctrl-C or activity resumes recording” 因此是假的：活动根本没人看。本机上的痕迹是最后一行
  索引 `2026-09-25 21:59:41`、最后一个视频 `22:43`，之后 14 小时零行，而 `windcapctl status` 一直报
  `recordable=true`、变化检测也正常。现在暂停期间每 10 秒问一次输入空闲（`winstate::snapshot` 本来就取了），
  `segment::resumed_from_input` 判“上一次看它以来有人动过”就恢复并重置变化门；量不到输入时也恢复——丢一次暂停只费电，
  丢一次恢复费的是一整天。计数 `resumed_from_pause` 进了收尾那行，因为只看 `paused_ticks` 的话，一个再也不醒的录制器
  看起来完全健康。
  Something that could erase a whole day is fixed: after a screen sat still past the five-minute threshold the recorder
  never woke up again. The paused branch closed its segment, ran maintenance, slept, and returned — while the only thing
  that ever clears the pause sits *after* the grab it had stopped doing, so once the segment was no longer committable
  nothing could lift `paused`. The line it printed, "Ctrl-C or activity resumes recording", was a promise nothing kept:
  activity was never looked at again. On this machine the trace was a last indexed row at 21:59:41, a last video at
  22:43, and fourteen hours with no rows while `windcapctl status` reported a recordable desktop and the change gate
  still found differences. A pause now asks the input-idle probe it already had in hand every 10 seconds, and
  `segment::resumed_from_input` ends the pause when anything was touched since the last look — resetting the change
  gate, because the frame it remembers predates the break. Unmeasurable input resumes too: a missed pause costs fan
  time, a missed resume costs the day. `resumed_from_pause` is in the closing report, because a recorder that never
  wakes up looks perfectly healthy in `paused_ticks` alone.
- AI 页上那五个 MCP 键是真的，可“改端口”以前不等于“换端口”：托盘只比对开关与在不在跑，所以你把 21120 改成 21121，旧进程
  还在 21120 上答话，屏幕上没有一处会说这件事——录制停了看得见，一个仍守在老地址上的服务看不见。现在托盘记住桥接是用哪组
  设置起来的（`BridgeSettings`，由 `wind_mcp::runtime::Runtime` 读，也就是服务自己启动时用的那个读者），刷新配置时发现
  host、port、auth、token 任一项变了就重启自己那个子进程；从终端起的 `windmcp serve` 不动，因为托盘从不杀自己没句柄的进程，
  改成说一次“现在要的是 127.0.0.1:21121，跑着的那个不是我起的”。五个键下面多了一行桥接自己的回答：开没开、此刻有没有人
  在听、听在哪个地址、该粘给助手的 URL，以及它若根本不肯启动，原样印出它那句拒绝。写入这一侧接上了同一个判据  `auth::startup_guard`：开关打开而令牌不足 24 字、或在非回环地址上关掉 Bearer，页面拒绝落盘并引用桥接的原话，不再让人存下
  一个永远起不来的配置。开关关着时不拦——那是休眠的配置，不是坏的配置，拦它会把整页别的编辑一起堵死。打开开关的那一次，页面
  一秒六之后再重读一回：这一行讲的是托盘按自己的节拍拉起的那个进程，保存后立刻重读会指着已经打开的端口说“未在运行”。实测：在
  窗口里打开开关并填令牌，`127.0.0.1:21120` 应声而开，行里出现“正在监听”与 URL；把端口改成 21121 保存，托盘几秒内把服务从
  21120 迁到 21121（旧端口关闭、`windmcp` 换了 pid）；关回开关，监听消失、锁文件收回。测完这台机器回到出厂姿态：off、21120、
  令牌清空。
  The AI page's five MCP keys were real, but changing the port did not move the port: the tray compared only "wanted" with
  "running", so a move from 21120 to 21121 left the old process answering on 21120 with nothing on screen to say so — a
  recorder that stopped is visible, a server still sitting on the old address is not. The tray now remembers the settings
  its bridge was started with (`BridgeSettings`, read through `wind_mcp::runtime::Runtime`, the same reader the service
  uses at startup) and restarts its own child when host, port, auth or token move. A `windmcp serve` started from a
  terminal is left alone — the tray kills nothing it holds no handle to — and is named once instead. Under the five fields
  the page now carries the bridge's own answer: switched on or off, whether anything is answering right now, the address it
  binds, the URL to paste into an assistant, and, if it would not start at all, that refusal word for word. Writing goes
  through the same predicate, `auth::startup_guard`: an enabled bridge with a token under 24 characters, or the bearer gate
  off on a non-loopback bind, is refused with the service's own sentence rather than saved as a configuration that can
  never come up. A switched-off bridge is not refused — a dormant setting is not a broken one, and blocking it would block
  every other edit on the tab.
  The page re-reads itself 1.6 s after a save on this tab, because the row describes a process the tray starts on its own
  timer — reading it immediately would say "not running" about a port that opens a moment later. Measured live: switching
  the bridge on and typing a token opened `127.0.0.1:21120` and the row read "listening" with the URL; moving the port to
  21121 had the tray migrate the service within seconds (old port closed, a new `windmcp` pid); switching it back off
  closed the listener and released the lock. This machine was left in its shipped posture — off, 21120, token cleared.
- 「一天之时」的当日柱状图从来没有画出来过：柱子的高度写成百分比，而它所在的那一列高度由内容决定，浏览器就把这个百分比当作
  `auto` 处理——240 根柱子实测全部 0 px 高；同时 4 px 的列间距在 240 列上是 956 px，比面板宽出 266 px，每列被压到 0 px，于是
  这一栏只剩一串重复的小时刻度。现在每列撑满那 7 rem 的行高、柱子绝对定位贴在底部、列间不留间距，小时轴单独一行、按每个小时真正
  占用的列数加权。同一个日子（2026-09-26，1276 行、活跃 6.36 小时）实测：修复前 0 根可见、图行宽 3287 px；修复后 52 根可见、
  最高 112 px、溢出 0 px、24 格小时轴与柱列边界误差 0.29 px。这一栏的标题也不再借用页面自己的名字「一天」，而行数角标跟着语言走
  （此前在中文界面里印 "1276 rows"）；「标记」为空时会说出它的行来自托盘的旗标命令，而不是只留一个破折号。
  The day screen's activity chart had never painted a bar: each bar's height is a percentage, and the column holding it is
  auto-height, so the browser treats that percentage as `auto` — measured 0 px tall on all 240 columns — while 4 px of gap
  across 240 columns is 956 px, 266 px wider than the panel, squeezing every column to 0 px and leaving the block showing
  nothing but repeated hour digits. Every column now fills the row's 7 rem, each bar is absolutely placed at its bottom, the
  columns carry no gap, and the hour axis is its own row weighted by the columns each hour really holds. Measured on the same
  day (2026-09-26, 1276 rows, 6.36 active hours): 0 bars visible and a 3287 px-wide row before, 52 bars visible, tallest
  112 px, 0 px overflow and a 24-cell axis agreeing to within 0.29 px after. The block's caption no longer borrows the page's
  own name, the row-count pill is translated (a Chinese install printed "1276 rows"), and an empty marks panel now says its
  rows come from the tray's flag command instead of leaving a bare dash.

> [!NOTE]
> 设置项覆盖范围尚未与旧的 Python 版对齐：设置页暴露 15 个键、录制页 25 个、AI 页 15 个，原版约有 100 个。缺的那些
> 是因为原生引擎目前还不读它们——界面会直接说明，而不是放一个不起作用的开关。录制深链（deep linking）是其中之一。
> 另：重绘预览图会按当前宽度增大索引文件（512 px、质量 70 下一张 1920×1080 的截屏约 7-12 KB，也就是每千行 7-12 MB；
> 带鱼的超宽屏截图会更贵）。把 `thumbnail_generation_size_width` 调小再跑一次 `windmaint previews` 就能缩回去。
> Settings coverage is not yet at parity with the Python app: the Settings page exposes 15 keys, Recording 25 and the
> AI page 15, where upstream offered roughly a hundred. The missing ones are missing because the native engine reads
> nothing for them yet — the window says which, rather than offering a switch that does nothing. Recording deep links is
> one. Also: redrawing previews grows the index at the configured width — measured on this install's own frames, a
> 1920×1080 grab is 7-12 KB at 512 px / quality 70, so seven to twelve megabytes per thousand rows, and a tall
> multi-monitor grab costs more. Setting `thumbnail_generation_size_width` back down and running `windmaint previews`
> again shrinks it.

--------------------------------------------------------------------------------

下面的条目是被本引擎替换掉的 Python 版自身的历史，一直到 0.0.31 为止。它们描述的 `main.py`、`record_screen.py` 和
`install_update.bat` 都不在这个产品里；保留它们是因为某个已迁移安装的数据是由它们写下的。
The entries below are the Python application's own history, up to and including 0.0.31. They describe `main.py`,
`record_screen.py` and `install_update.bat`, none of which is in this product; they are kept because a migrated
install's data was written by them.

## 0.0.31
> 2025-03-16
- 新增选项：捕风记录仪在删除临时视频与文件时，支持直接删除、而非先放到回收站由系统定期删除，从而降低磁盘占用；New configuration option: When deleting temporary videos and files, windrecorder can now delete them directly instead of putting them in the Recycle Bin and letting the system delete them regularly, thereby reducing disk usage;
- 添加了 CPU 压缩编码时的最大线程参数，当使用 CPU 压缩视频时，可以降低持续的高负荷占用时间、避免系统卡顿；Added the maximum thread parameter for CPU compression encoding. When using the CPU to compress video, it can reduce the continuous high load time and avoid system lag; (@RTLiang) #206

--rollout to new user--
- 新增插件 LLM_search_and_summary：可以使用AI（大语言模型）对记录的内容进行自然语言搜索与总结了；New plugin LLM_search_and_summary: AI (Large Language Model) can be used to perform natural language search and summary of recorded content; (@yuansui486) #277

---

## 0.0.30
> 2025-01-29
- 在 OCR 搜索时，会高亮显示 OCR 结果和其中的关键词；During OCR search, the OCR results and the keywords in them will be highlighted; #262

![instruction-ocr-highlight](https://github.com/yuka-friends/Windrecorder/blob/main/__assets__/instruction-ocr-highlight.jpg)


### Fixed
- 修复了当db文件夹中有其他文件（如未释放的临时数据库时）可能报错阻塞无法启动程序；Fixed the issue that when there are other files in the db folder (such as unreleased temporary databases), the program may be blocked and unable to start; #257

---

## 0.0.29
> 2024-12-01
- 支持搜索中同时包含窗口标题和内容，以更准确地筛选过滤结果；Supports including both window title and content in the search to filter the results more accurately;

### Fixed
- 在定位搜索结果时，如果数据库中定位时间戳错误小于视频开始时间戳时，自动回正到第1秒；When locating search results, if the locating timestamp in the database is less than the video start timestamp, it will automatically return to the first second;
- 正确捕获 LLM 生成出错时的异常；Handle exceptions when LLM generation fails; (@X-T-E-R)
- 统计页跨月时，LLM 标签按钮可能因 key name 冲突而报错；When the statistics page spans across months, the LLM label button may report an error due to key name conflict;
- 在保存旗标表格时，因使用了过时的 experimental rerun 而导致报错；When saving the flag table, an error occurred due to using the outdated experimental rerun;
- 修复了创建开机快捷方式可能导致程序崩溃的问题；Fixed an issue where creating a startup shortcut could cause the program to crash;（@zhentouyu）#162

---

## 0.0.28
> 2024-09-24
- 添加了自定义 webui 背景图功能，可以在 `extension/set_custom_webui_background` 设置；Added custom webui background image, can be set in `extension/set_custom_webui_background`;
- 🍃 细化 灵活截图模式 的节能策略选项，可以选择立即合成、仅在插电时合成（限笔记本电脑）、仅在电脑空闲时合成视频；修复了 PC 由于一直插电、导致无法设定为仅闲时合成视频的问题；Refine the energy-saving strategy options of Flexible Screenshot Mode, you can choose to synthesize immediately, synthesize only when plugged in (limited to laptops), and synthesize video only when the computer is idle; fix the problem that the PC cannot be set to synthesize video only when idle because it is always plugged in; https://github.com/yuka-friends/Windrecorder/issues/237
- 提高了 webui 在记录数据较多时的初始化速度，通过优化了 check_is_onboarding 的判断逻辑；Improved the initialization speed of webui when recording a lot of data by optimizing the judgment logic of check_is_onboarding;

![set_custom_webui_background](https://github.com/yuka-friends/Windrecorder/blob/main/extension/set_custom_webui_background/_preview.jpg)

---

## 0.0.27
> 2024-09-17
- 添加了是否启用记录浏览器链接的选项，如果感到浏览器卡顿，可以尝试关闭；Added the option to enable recording browser links. If you feel the browser is lagging, you can try to turn it off;

### Fixed
- 修复了在启动一段时间后，当前台窗口标题包含 windrecorder 时可能会被错误隐藏的 bug；Fixed a bug where the foreground window might be hidden incorrectly when its title contains windrecorder after a while of startup;

---

## 0.0.26
> 2024-09-10
- 更新了隐藏命令行窗口的方式，现在可以一打开 start_app.bat 立即自动隐藏；Updated the way to hide the command line window. Now it can be automatically hidden immediately after opening start_app.bat;
    - 如果命令行窗口一闪而过、过了一段时间 捕风记录仪 仍没有出现在托盘中，这可能是由于新的隐藏方式与系统不兼容，请在目录下创建一个名为`hide_CLI_by_python.txt`的文件以回到原先兼容性更加的隐藏方式；
    - If the command line window flashes by and the Windrecorder still does not appear in the tray after a while, it may be because the new hiding method is incompatible with the system. Please create a file named `hide_CLI_by_python.txt` in the directory to return to the original hiding method with more compatibility; https://github.com/yuka-friends/Windrecorder/issues/232

---

## 0.0.25
> 2024-09-01
- 生成光箱图片时，可以选择在底部添加时间戳水印；When generating a lightbox image, you can choose to add a timestamp watermark at the bottom;
- 添加了自定义光箱缩略图生成器，可以从任意日期范围创建光箱图片，支持自定义缩略图数量、分布模式、图像大小等。你可以在 `extension\create_custom_lightbox_thumbnail_image` 进行使用；Added custom lightbox thumbnail generator, which can create lightbox images from any date range, supports custom thumbnail count, distribution pattern, image size, etc. You can use it in `extension\create_custom_lightbox_thumbnail_image`; https://github.com/yuka-friends/Windrecorder/issues/226

![create_custom_lightbox_thumbnail_image](https://github.com/yuka-friends/Windrecorder/blob/main/extension/create_custom_lightbox_thumbnail_image/_preview.jpg)

### Fixed
- 修复了 json 可能因为未指定 utf-8 编码而导致读取出错的潜在问题；Fixed a potential issue where json might be read incorrectly because utf-8 encoding was not specified;

---

## 0.0.24
> 2024-08-24
- 在全局搜索页，提供更方便操作的月份滑杆选择器、精确日期选择器两种筛选模式；On the global search page, two filtering modes are provided: month slider selector and precise date selector, which are more convenient to operate;
- 在将每日活动提交给语言模型前，可以排除特定的词语列表，从而降低因为敏感内容导致生成失败概率；Before submitting daily activities to the language model, a specific list of words can be excluded to reduce the probability of generation failure due to sensitive content;
- 在每月活动中，支持通过语言模型生成标签、查看当月的所有诗词；In monthly activities, you can generate tags through language models and view all poems of the month;

---

## 0.0.23
> 2024-08-19
- 支持使用大语言模型为每日活动写一句诗歌；Add the ability to write a poem for each activity;
- 优化灵活截图模式，当一段时间（默认2mins）在跳过规则时终止记录当前视频片段；Optimize the flexible screenshot mode, and stop recording the current video clip when the rule is skipped for a period of time (default 2 minutes); (config.screenshot_interrupt_recording_count)

---

## 0.0.22
> 2024-08-17
- 支持使用大语言模型对每日活动进行标签提取总结；Add the ability to extract and summarize tags for daily activities;

![header_llm](https://github.com/yuka-friends/Windrecorder/blob/main/__assets__/header_llm.png)

- 升级 streamlit 版本，优化了部分界面布局；Upgrade the version of streamlit to optimize some interface layouts;
- 优化截图文件夹清理机制：清理之前可能遗漏的数据不足文件夹；Optimize the screenshot folder cleaning mechanism: clean up the folders with insufficient data that may have been missed before;
- 添加空闲时清理缓存文件夹机制；Add the mechanism of cleaning the cache folder in idle time;
- 将统计中的年月散点图缓存移动至用户文件夹；Move the scatter plot cache of year and month in statistics to the user folder.
- 支持使用"-"连接单词进行连续整句话的搜索匹配。比如通过搜索 'i-love-you' 而不是 'i love you'来匹配连续整句；Support using "-" to connect words for continuous whole sentence search matching. For example, by searching for 'i-love-you' instead of 'i love you' to match continuous whole sentences;

![instruction-search-split-dash](https://github.com/yuka-friends/Windrecorder/blob/main/__assets__/instruction-search-split-dash.jpg)

### Fixed
- bug: 当程序目录存在空格时，第三方 OCR 扩展无法进入虚拟环境完成安装；When the program directory has a space, the third-party OCR extension cannot enter the virtual environment to complete the installation;
- bug: 当时间戳位于12月，换算后结果可能大于12月从而异常；When the timestamp is in December, the result after conversion may be greater than December, which is abnormal; (utils.py(278))
- bug: 由于 wechat ocr 重复初始化导致 webui 假死；Due to repeated initialization of WeChat OCR, WebUI freezes;

---

## 0.0.21
> 2024-08-06
- 添加以下第三方 OCR 扩展，可以在 Extension 目录下进行安装；Add the following third-party OCR extensions, which can be installed in the Extension directory;
    - RapidOCR (Paddle OCR based on ONNXRuntime) (Collaborators: ASC8384)
    - WeChat OCR (Collaborators: B1lli), with extremely high Chinese and English recognition accuracy;
    - Tesseract OCR, supports more than 100 languages ​​and can recognize multiple languages ​​at the same time;
- 为压缩视频添加自动硬件加速参数；Added automatic hardware acceleration parameters for compressed video;

### Fixed
- bug：当截图文件不完整或损坏时，webui未能捕捉阻塞报错；When the screenshot file is incomplete or damaged, the webui not capture the blocking error;

---

## 0.0.20
> 2024-07-13
- 添加对 Chrome、Microsoft Edge、Firefox 当前浏览的 url 记录；Add the recording of the currently browsed URL in Chrome, Microsoft Edge, and Firefox;

---

## 0.0.19
> 2024-07-12
- 增加对前台窗口进程名的记录，从而可以更精确地统计与排除应用；Added the record of the foreground window process name, so that applications can be counted and excluded more accurately;

---

## 0.0.18
> 2024-07-06

### Fixed
- bug：修复了在新的一个月「自动灵活截图」无法自动将截图缓存转换为视频；Fixed the issue where the "Automatic Flexible Screenshot" couldn't automatically convert the screenshot cache into a video in the new month.
- bug：优化时间轴截图生成算法，当使用「自动灵活截图-仅捕捉前台窗口」时降低预览图变形几率；Optimized the timeline screenshot generation algorithm, which reduces the chances of preview image deformation under the "Automatic Flexible Screenshot - foreground window only" recording mode;
- 自动移除空截图缓存文件夹；Automatically remove the empty screenshot cache folder;
- bug: 修复了当录制视频时长超出 config.record_seconds 时，无法在一天之时中被定位展示；Fixed the issue that when the duration of the recorded video exceeds config.record_seconds, it cannot be located and displayed in Oneday;
- bug：一天之时在寻找最早最晚截图时间戳时，数据为空时会可能导致报错；When searching for the earliest and latest screenshot timestamps at Oneday, it may cause an error when the data is empty;
- bug：修复错误记录的维护时间戳缓存可能会阻塞正常的录制线程；Fixing the error maintenance timestamp cache may block the recording thread;

---

## 0.0.17
> 2024-06-29

### Fixed
- bug：修复了「自动灵活截图」下可能无法记录当前时间戳旗标；Fixed the issue that the current timestamp flag might not be recorded under "Automatic Flexible Screenshot";
- bug：修复了「自动灵活截图-仅捕捉单显示器时」无法使用；Fixed the issue that the "Automatic Flexible Screenshot" recording mode would not work;
- bug：修复了当截图未完整存储时无法合成视频的情况；Fixed the situation that the video could not be synthesized when the screenshot was not completely stored;

---

## 0.0.16
> 2024-06-26
- 添加了「自动灵活截图」录制模式，现在能以更低的系统资源进行录制、实时回溯已录制画面了，同时可以仅录制前台窗口、精确过滤不想被录制的内容。同时也保留了原先的「直接录制视频（ffmpeg）」模式，可根据需要自行选择；
- Added "Automatic Flexible Screenshot" recording mode, which can now record with lower system resources and replay recorded images in real time. It can also record only the foreground window and accurately filter out the content you don't want to record. At the same time, the original "Directly record video (ffmpeg)" mode is also retained, and you can choose it according to your needs.

---

## 0.0.15
> 2024-06-09

### Fixed
- bug: 修复了锁屏检测在 Windows 11 上不起作用；Fixed lock screen detection not working on Windows 11;
- 优化 i18n 逻辑，当 key 不存在时会 fallback 到 English 文案；Optimizing i18n logic, when the key does not exist, it will fallback to English copy text;

---

## 0.0.14
> 2024-06-01
- 升级了图像嵌入模型到 unum-cloud/uform v3，模型不再依赖庞大的 torch 环境，而使用更加节能轻便的 ONNX 进行推理，速度、能耗与召回质量均得到提升。如果你之前安装了旧版本，可通过 extension/install_img_embedding_module 中的脚本先卸载旧版、再安装新版、并可以对旧数据进行回滚以重新索引；
- 压缩视频：添加 AMF（AMD Advanced Media Framework）编码器选项；(@arrio464)
- 添加针对 Microsoft Edge / Firefox 的 h265 解码提示；

- Upgraded the image embedding model to unum-cloud/uform v3. The model no longer relies on the huge torch environment, but uses the more energy-efficient and lightweight ONNX for reasoning, which improves speed, energy consumption, and recall quality. If you have installed an old version before, you can use the script in extension/install_img_embedding_module to uninstall the old version first, then install the new version, and roll back the old data to re-index.
- Compressed video: added AMF (AMD Advanced Media Framework) encoder option;(@arrio464)
- Added h265 decoding tips for Microsoft Edge/Firefox;

### Fixed
- bug: 当试图列出每月第一天的所有数据时，会因为首条记录预览图为 None 而阻塞报错；When trying to list all data on the first day of each month, an error will be reported because the preview image of the first record is None;

---

## 0.0.13
> 2024-05-18
- 为索引出错的视频文件添加自动重试机制；
- Add automatic retry for index errored video files;

---

## 0.0.12
> 2024-04-21
- 添加了月度统计中对窗口标题的过滤，现在可以查看具体关于某件事的屏幕时间了；
- Added filtering for window titles in monthly statistics, now you can view screen time specifically about something;

### Fixed
- bug: 当 OCR 支持语言找不到对应测试集时，将会阻塞 onboarding 向导；When the OCR supported language cannot find the corresponding test set, the onboarding wizard will be blocked;
- 添加更多尝试隐藏 CLI 窗口次数重试，以应对未解锁屏幕时隐藏失败；Added more retries to try to hide the CLI window in case hiding fails when the screen is not unlocked;
- Fix: Startup path now supports spaces;(@zetaloop)

---

## 0.0.11
> 2024-04-19

- 支持多显示器与单个显示器录制；
- 添加了录制时的编码选项（cpu_h264, cpu_h265, NVIDIA_h265, AMD_h265, SVT-AV1）；(@myshzzx)
- 优化了索引时比较图像的性能；
- 索引视频切片时，如果有相同内容显示在不同时间点，可以只记录第一次出现、而不重复记录；
- 优化 webui 底部统计信息缓存机制，在数据多的情况下获得更快加载体验；

- Supports multi-monitor and single-monitor recording;
- Added encoding options when recording (cpu_h264, cpu_h265, NVIDIA_h265, AMD_h265, SVT-AV1);(@myshzzx)
- Optimized the performance of comparing images during indexing;
- When indexing video slices, if the same content is displayed at different points in time, only the first occurrence can be recorded without repeated recording;
- Optimize the statistical information caching mechanism at webui footer to obtain a faster loading experience when there is a lot of data;

### Fixed
- bug: 当锁屏时程序有几率不会进入空闲暂停状态；There is a chance that the program will not enter the idle pause state when the screen is locked;
- bug: INDEX 标签被添加在 iframe cache 目录名中，导致不会被 img embedding 索引和清理；The INDEX tag should not been added to the iframe cache directory name, which resulting in it not being indexed and cleaned by img embedding;

---

## 0.0.10
> 2024-03-03

### Fixed
- https://github.com/yuka-friends/Windrecorder/pull/138 feat: 为 webui footer 数据统计添加了缓存机制，不需要每次进入 webui 都进行统计了，大幅提高加载速度。 A caching mechanism has been added for webui footer data statistics, so there is no need to perform statistics every time you enter webui, which significantly improve loading speed.
- bug: 当一天以非零点分隔时，每月最后一天无法被选择。 The last day of the month cannot be selected when the days are separated by a non-zero point.
- bug: 升级时没有保留原用户 config. The original user config is not retained during the upgrade.
- bug: bug: during CLI loading, if force on other window it be hidden instead of CLI https://github.com/yuka-friends/Windrecorder/issues/133

---

## 0.0.9
> 2024-03-02

### Fixed
- https://github.com/yuka-friends/Windrecorder/pull/137 bug: 在索引视频时因为 column name typo 导致索引失败。 Indexing failed due to column name typo when indexing videos.

---

## 0.0.8
> 2024-02-24

- **添加图像语义嵌入、检索扩展，可以通过对画面的自然语言描述进行搜索、或以图搜图**；在 extension\install_img_embedding_module 进行安装；
- 可跳过录制自定义的前台活动了，比如自定义锁屏、游戏、隐私场景等；
- 移除了 一日之时 中的词云统计，UI 提供双栏与三栏布局；
- 在搜索时提供推荐词；
- 调整目录结构，将所有用户数据集中放置到 userdata 下；
- 添加日志；添加对搜索历史的记录（可在 config_user.json enable_search_history_record 中设置）
- 对闲时任务添加了分批数量限制；

- Add image embedding and retrieval extension, which can **search through the natural language description of the picture or search for pictures**; install in extension\install_img_embedding_module;
- You can now skip recording customized foreground activities, such as customized lock screens, games, privacy scenes, etc.
- Removed wordcloud in Daily; Provide two-column and three-column layout;
- Provided synonyms recommend in global search; 
- Adjust the directory structure and centrally place all user data under userdata dictionary;
- Add logger; Add a record of search history (can be set in config_user.json enable_search_history_record)
- Added a limit on the number of batches for idle tasks;

### Fixed
- bug: 当 CLI 未隐藏时切换了活动前台窗口、导致该前台窗口被隐藏；(?)

---

## 0.0.7
> 2024-02-09

- **添加对窗口标题的记录与统计；**
- 添加时间标记功能，可以为当下时间、回溯中的时间进行标记和备注，方便回忆查找；
- 添加 webui 保护密码；
- 添加托盘中的更新日志入口；
- 隐藏显示命令行窗口，加入阻止托盘重复运行；
- 升级支持了 python 3.11；虚拟环境默认创建在 windrecorder 目录下；

- Added time mark function, which can mark and make notes for the current time and the time in retrospect to facilitate recall and search;
- Added records and statistics of window titles;
- Add webui protection password;
- Added update log entry in the tray;
- Hide the command line window and prevent the tray from running repeatedly;
- Upgraded to support python 3.11; the virtual environment is created in the windrecorder directory by default;

### Fixed
- https://github.com/yuka-friends/Windrecorder/issues/109 bug: 托盘显示"WebUI started failed"且无webui错误日志
- https://github.com/yuka-friends/Windrecorder/issues/97 bug: 当开启“形近字”搜索，可能以空字符进行搜索
- https://github.com/yuka-friends/Windrecorder/pull/105 修复图像对比函数
- https://github.com/yuka-friends/Windrecorder/issues/100 bug: “记忆摘要”统计中，翻阅年视图不生效
- https://github.com/yuka-friends/Windrecorder/pull/88 fix state page date selector

---

## 0.0.6
> 2024-01-25

- **修复了 webui 下重复轮询数据库的 bug，大幅提升使用性能；**
- 修复全新安装时可能遇到的 db 目录不存在而阻塞的错误；
- 移除更新 st.experimental.rerun 为 st.rerun；
- 优先在 Windrecorder 目录下添加虚拟环境；

- **Fixed the bug of repeatedly polling the database under webui, greatly improving performance;**
- Fixed an error that may be encountered during a new installation due to the non-existence of the db directory;
- Removed and updated st.experimental.rerun to st.rerun;
- Prioritize adding a virtual environment under the Windrecorder directory;

### Fixed
- https://github.com/yuka-friends/Windrecorder/issues/103 bug: update check broken, user can never get update remind
- https://github.com/yuka-friends/Windrecorder/issues/100 bug: “记忆摘要”统计中，翻阅年视图不生效
- https://github.com/yuka-friends/Windrecorder/issues/87 bug: 当存在跨年的数据，“记忆摘要”tab 下的月份选择器范围约束会失效
- https://github.com/yuka-friends/Windrecorder/issues/77 bug: 在托盘关闭 webui 服务后，菜单残留局域网提示项
- https://github.com/yuka-friends/Windrecorder/issues/56 feat: 已有托盘在运行时，阻止托盘重复启动、且提供指引提示

---

## 0.0.5
> 2023-12-22

- 添加了托盘形态：现在可以通过托盘来控制记录与进入查询页面了，不再需要通过脚本手动控制；
- 为闲时维护添加了视频压缩参数设置；
- 使用 poetry 创建与维护虚拟环境；

- Added tray: now you can control recording and enter the query page through the tray, no longer need to manually control through script;
- Added video compression parameter settings for idle time maintenance;
- Use poetry to create and maintain virtual environments;

### Fixed
- https://github.com/yuka-friends/Windrecorder/issues/75 bug: 点击webui中 录制与视频存储-自动化维护-测试支持的编码方式 之后，报错，详细信息如图
- https://github.com/yuka-friends/Windrecorder/issues/62 Bug: webui 在 Firefox 上无法播放，本地可以
- https://github.com/yuka-friends/Windrecorder/issues/37 bug: Windows Terminal 下背景颜色显示异常

---

## 0.0.4 
> 2023-11-25

- 修复若干 bug，治理重构了大量代码。

- Fixed bugs and refactor a large amount of code.

### Fixed
- https://github.com/yuka-friends/Windrecorder/issues/42 执行install_update_setting.bat报错Type error
- https://github.com/yuka-friends/Windrecorder/issues/33 random walk功能失效
- https://github.com/yuka-friends/Windrecorder/issues/31 记忆摘要部分中的词云和光箱无法正确更新
- https://github.com/yuka-friends/Windrecorder/issues/27 安装时候报错
- https://github.com/yuka-friends/Windrecorder/issues/22 文件夹在重新命名后未考虑到原本的文件夹问题
- https://github.com/yuka-friends/Windrecorder/issues/12 在全局搜索页面时，有时输入框需要输入两次才能执行搜索
