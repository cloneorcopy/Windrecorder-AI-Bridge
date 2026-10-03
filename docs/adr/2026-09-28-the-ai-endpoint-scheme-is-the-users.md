# AI 端点的传输方案由用户决定，产品不再拦明文 HTTP

日期：2026-09-28
分支：`perf/algorithms`
状态：已确认并已落地（本决定不可逆地改变出站策略，故留此记录）

## 背景

`windcap/ai/src/client.rs` 的 `check_url` 在 `ask` 里、在任何 socket 之前判断：`http://` 只有主机是回环地址才放行，否则返回 `Unconfigured`，句子是

> `open_ai_base_url` is `…`: plain HTTP would carry `open_ai_api_key` across the network unencrypted. Use https:// for a hosted endpoint, or point at a loopback address for a local one.

写它的理由是"复制粘贴错一个地址就把令牌发到了公网"。这个理由在**云端接口**上成立，但它把另一大类用户挡在了外面：自建网关。这台开发机的实测就是那一大类——

* `userdata/config_user.json`：`open_ai_base_url = http://192.0.2.10:3321/v1`（同一局域网里的 new-api 网关）。
* `windai doctor`：`round trip : failed after 0.0s`，上面那句拒绝。一个字节都没发出去。
* 用同一地址、同一令牌直接打：`GET /v1/models` 200 / 55 ms，`POST /v1/chat/completions` 200 / 1023 ms。

也就是说网络、密钥、端点、模型名全都是好的，唯一挡住的是产品自己的一条规则；而这条规则报出来的是一句配置错误，用户读到的是"AI 栏目所有试一试都失败"。受影响的入口不止按钮：`ai_test`、七条提示词的 `prompt_trial`、空闲轮里的 `ai-tags` 与 `ai-summaries`、以及经 MCP 桥发起的总结，全部走同一个 `Client::ask`，全部在同一处失败。

设置页那行说明（`ai_help_base_url`）当时写的是"除本机以外只接受 https"，所以这条规则对用户是可见的——但它可见的方式，是让用户去改一个他改不了的地址。

## 决定

删掉 `check_url` 及其在 `ask` 里的调用。`open_ai_base_url` 写的是什么方案就按什么方案发：`http://` 发给局域网网关，`https://` 发给云端接口，明文出网的代价由写下这个地址的人在写下的那一刻判断，程序不再二次猜测。

不新增配置键，不加"私有地址段"白名单，也不加"允许明文"复选框。三条都审过并否掉：

* 私有段判定（RFC1918 / CGNAT / link-local / `.local`）看着温和，实际是把"哪些网络算可信"变成产品定义，而酒店、办公室、校园网同样是"私有段"，风险一分不少，用户却多了一条无法解释的规则。
* 新增开关会把一个本该由地址本身表达的选择，拆成两处必须同时正确的状态——正是本项目反复付代价的形状（门禁围着功能修）。
* 保持默认拒绝、只改文案，等于承认这个能力对本 fork 的主要人群不可用。

保留的两件事与这条规则无关：`Settings::require_usable` 仍然要求地址、密钥、模型名齐备且方言匹配（那是"能不能发"而不是"该不该发"）；`http.rs` 的 `is_loopback` 仍然在用，但它现在只服务一个决定——回环会话以 `NO_PROXY` 打开，不被机器上的代理配置吞掉。

## 后果

* 局域网自建网关开箱可用：`windai doctor` 实测 `round trip : 1.9s, 4 characters back`，AI 页的测试与七条试一试随之可用。
* 明文地址的令牌确实会明文过网。这是本决定接受代价，不是疏漏；三语文案（`ai_help_base_url`）与 `windui/src/ai.rs` 里那行英文说明都改成把这句话讲给使用者听，而不是替他拒绝。
* 出站策略不再是 `wind-ai` 的一条规则，因此"谁在空闲轮里发出请求"这个问题，答案只剩 `require_usable` 与 AI 页那两个 `*_in_idle` 开关——门禁不在其中，以前它也不该被当作门禁。
* 钉住旧行为的三处断言已随之改写，而不是删掉了事：`client.rs` 现在用记录 URL 的桩断言"明文地址被发出去了"，`windui/src/ai.rs` 从页面侧断言同一件事，`commands.rs` 里那条"试一试不泄密钥"的测试改用无方案地址来保持"未发字节"这一前提。
