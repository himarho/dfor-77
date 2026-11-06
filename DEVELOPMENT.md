## 開発者向けドキュメント

### 名称と境界

`dfor-77` は原則として、ANSI X3.9-1978に従うBotプログラムから、`INCLUDE` や拡張構文なしで利用可能なAPI群およびruntime連携機構を指す。`dfor-77` という単体表記はこの本体だけを指し、`dfor-77 extension` を含めた全体を指す名称としては基本的に使わない。全体に言及する必要がある場合は、`dfor-77` と `dfor-77 extension`、またはこのプログラム全体と書く。

`dfor-77 extension` は便利化と柔軟化のための拡張層であり、このプログラム上で提供可能なDiscord機能の必須到達経路にはしない。

### 目的

dfor-77は、FORTRAN77からDiscord Botを作るためのruntimeとFORTRAN wrapperである。Bot本体は固定形式FORTRANでeventを受け取り、message送信、削除、timeout、interaction応答、rich payload送信、REST操作などを `D7` APIで呼ぶ。Discord Gateway接続、REST rate limit、権限判定、snapshot、監査ログ、mock検証はRust runtime側が引き受ける。


### 要件

1. `dfor-77` は，Discord Botのevent処理，REST操作，権限判定，監査記録をFORTRAN77から扱えるBot作成体験を提供することを目標とすること．
2. `dfor-77` でいうFORTRAN77は、ANSI X3.9-1978標準と、それに従うプログラムをいうこと。
3. ANSI X3.9-1978に従うBotプログラムが、このプログラム上で提供可能なDiscord機能を `dfor-77 extension` に依存せず利用できること。
4. `dfor-77 extension` は、プログラム作成を容易または柔軟にするため、`dfor77.inc`、便利wrapper、拡張サンプル等を提供できること。
5. `INCLUDE` 文等のFORTRAN77標準に含まれない機能を使うものは、`dfor-77 extension` として明示すること。
6. `dfor77.inc` の内容は、FORTRAN77標準環境で手動展開して利用できる形式で提供すること。

### v0.3.0の範囲

| 領域 | 到達点 |
| --- | --- |
| FORTRAN API | 初期化、event取得、getter、snapshot utility、preflight、action、error/log、payload builder |
| Gateway | Identify、Resume、Heartbeat、READY、MESSAGE、INTERACTION、REACTION、MEMBER join/leave、unknown dispatch、rate limit、presence/voice state enqueue |
| REST | message、attachment、sticker、poll、interaction、application command、typing、reaction、timeout、guild/channel/role/member系操作、raw JSON REST |
| Snapshot | guild、channel、thread、message、member、role、user tombstone、TTL、max entries、gap時のstale化 |
| Permission | ADMINISTRATOR、guild owner、channel overwrite、private thread access、role hierarchy、send/delete/timeout判定 |
| Audit | token/header非出力、content保存方針、HMAC-SHA256、redaction、JSON Lines、検証CLI |
| Supervisor | Bot process監督、multi-shard、shard別restart、hot restart、Resume session保存 |

### 公開判定

現時点の実装は，ローカルmockとFORTRAN acceptance上では非Voice管理Bot向けMVPに到達している．公開リポジトリとしては，公開対象fileの分離，security response体制，versioned release note，API差分追従手順を明示する．Discord staging環境で実際に接続する稼働確認は公開前の運用確認として行うが，検証記録を公開配布物として保存しない．`SECURITY.md` と `CHANGELOG.md` は同梱し，v0.3.0はdevelopment previewとして扱う．

| 判定軸 | 現状 | 公開前の条件 |
| --- | --- | --- |
| API到達性 | 主要RESTとGateway操作は `D7` APIまたは `D7RST` で到達可能 | Discord staging環境で公開直前に稼働を確認する |
| 管理Bot基盤 | message moderation，timeout，ban，role，reaction，webhook，audit，Auto Moderation rule，application commandに到達可能 | 永続DBやdashboardを持つBotアプリケーション側の設計を分離する |
| セキュリティ | token redaction，audit policy，REST origin制限，添付file検査，security issue受付を持つ | 実token，private guild data，運用logを公開対象から除外する |
| 配布品質 | package whitelist，source policy，license checkを持つ | clean checkoutで `make publish-check` を通す |

### Discord公式仕様との照合

確認日: 2026-07-03．Discord Developer DocumentationのGateway，Gateway Events，Rate Limits，Permissions，Interactions，Application Commands，Voice，HTTP API Resourcesを基準に，READMEの「プログラムの目的」と実装の合致度を評価する．この評価は開発者向けの棚卸しであり，利用者向け機能保証ではない．

| Discord仕様上の要点 | 実装上の到達点 | 弱い点または対象外 | 目的への合致 |
| --- | --- | --- | --- |
| BotはGateway WebSocketでeventを受信し，Identify，Heartbeat，Resume，sequence管理，intent指定を正しく扱う必要がある | v10 JSON Gateway，Identify，Resume，Heartbeat，READY，MESSAGE，INTERACTION，REACTION，MEMBER系event，unknown dispatch，4096 byte送信上限，outbound送信予算，daily Identify予算，Resume session保存を持つ | zlib-stream compression，ETF encoding，公式event全種の個別FORTRAN event化は未完 | FORTRAN77 Botからevent loopを書く目的に強く合致 |
| Gateway intentsはv8以降必須で，MESSAGE_CONTENT，GUILD_MEMBERS等のprivileged intentはDeveloper Portal側の許可を必要とする | 受信対象に応じたintent設定を受け取り，guild messageやmember情報が必要な構成では `MESSAGE_CONTENT` と `GUILD_MEMBERS` がない場合のruntime警告を持つ | privileged intent未許可による4014等はGateway fatal closeとして扱うが，設定診断はPortal状態を直接検査できない | 目的に合致するが，運用手順で補う領域が残る |
| REST APIはroute bucket，global limit，429 response，invalid request制限を考慮し，hard-codeした固定limitへ依存しない必要がある | `X-RateLimit-Bucket`，`Retry-After`，`X-RateLimit-Reset-After`，global 429，50 rps token budget，invalid request budgetを扱う | endpoint固有の非標準rate limit，emoji等の特殊挙動，Discordと実際に接続した長時間差分検査は未完 | runtimeがREST負荷を肩代わりする目的に合致 |
| Permissionはguild base permission，channel overwrite，ADMINISTRATOR，role hierarchy等を区別する必要がある | snapshotとREST refreshを組み合わせ，send/delete/timeoutについてowner，ADMINISTRATOR，channel overwrite，thread状態，role hierarchyを判定する | 全操作のpermission preflightは網羅していない．`D7RST` はtyped APIのpreflightを迂回するため，信頼済みBot code向け低レベルAPIとして扱う | 管理Bot基盤として概ね合致．raw RESTの扱いは明示が必要 |
| InteractionはGateway受信でもHTTP callbackで3秒以内にinitial responseを送る必要があり，tokenはfollowupに使う | `INTERACTION_CREATE` 受信，`D7ICB` initial callback，`D7IFU` followup/original response，application command CRUDを持つ．現在処理中のinteractionでDiscordが `10062 Unknown interaction` を返し，かつ3秒を超過している場合は `D7TIM` / `INTERACTION_DEADLINE_EXCEEDED` として判別する | runtimeは自動defer応答を送らない．公開HTTP interaction receiver，signature検証，inline HTTP responseは未実装 | Gateway型Botの目的に合致．HTTP receiver型Appは対象外 |
| Application Commandsはchat input，user，message commandと深いschemaを持つ | `D7ACR`，`D7SCM`，`D7SCX`，`D7CMD`，`D7XACB` でCRUDと基本payload生成に到達する | localization，contexts，integration types，autocomplete，command permissions等はraw payloadまたは追加builder対象 | 目的に合致するが，extension helperを増やす余地が大きい |
| Voice audioはGatewayとは別にVoice WebSocket，UDP/RTP，encryption，codecを必要とする | `D7VST` はGateway voice state payload enqueue，REST voice管理は `D7RST` で到達可能 | Voice WebSocket，UDP/RTP，audio encode/decode，voice send/receiveは未実装かつv0.3.0対象外 | 非Voice Bot runtimeとしては合致．音声Bot libraryとしては未達 |
| HTTP API ResourcesはBot tokenだけで完結しないOAuth2 user-token領域，Social SDK，monetization等を含む | Bot tokenで扱いやすいmessage，guild，channel，role，member，webhook，Auto Moderation，application command等を優先し，その他は `D7RST` と extension helperで到達性を保つ | OAuth2 user consent，Social SDK，Lobby，Incoming interaction HTTP serverは別設計が必要 | Discord Bot runtimeとして妥当な取捨選択 |

総合評価: READMEの目的である「FORTRAN77によってDiscord Botを作るためのruntimeおよびAPI wrapper」には，非VoiceのGateway型Bot，特に管理Botの基盤として概ね合致する．一方で，公式仕様の全機能を高水準typed APIで覆う段階ではない．現状の強みは，FORTRAN77 Botから主要Gateway eventと主要REST操作を呼べること，rate limit，permission，snapshot，auditをRust runtime側へ寄せていることである．残る差分は，Gateway eventの個別化，REST typed wrapperの拡張，raw RESTの安全な運用境界，HTTP interaction receiver，Voice audioの明確な対象外化である．

### 完成度評価

音声通信本体，すなわちVoice WebSocket，UDP/RTP，音声encode/decode，voice send/receiveは，現時点の完成条件から外す．Gatewayへpresence payloadやvoice state payloadを積む経路はあるが，音声Bot libraryとしての完成度は評価対象にしない．

#### dfor-77

| 領域 | 現状 | 未達成または弱い点 | 評価 |
| --- | --- | --- | --- |
| FORTRAN公開API | `D7` prefix、英数字6文字以内、固定形式FORTRAN wrapperで主要操作を提供する | 全REST endpointの専用typed wrapperはまだ網羅していない | 非Voice Bot向けMVP到達 |
| 標準Botからの利用 | `examples/sample_moderation/sample_moderation.f` は `INCLUDE` なしでbuildし，runtime acceptanceでmessage削除，timeout，記録投稿を検査する．さらにINCLUDEなし固定形式Botで主要APIカテゴリの呼び出し面，payload builderのJSON生成，sticker/poll/reaction/upload系action，JSON payload送信，単一添付，interaction callback，application command，typing，管理系REST wrapper全13種，raw REST，response body取得のIPC契約とmock REST副作用，REST 400/403/404/429/送信系5xx/read-only 5xxのstatus/error reader mapping，Discord JSON error codeに基づく主要なnot found，permission，thread，message type，forum parent，rate limit，invalid bodyの意味分類，preflightの出力契約，ローカル拒否，runtime snapshot上の許可/拒否判定，forum parent channel，archived/locked public threadのsend拒否を検査するacceptanceを持つ | Discord公式の全error codeを個別typed APIごとに検査する範囲までは対象外．Discord staging環境で実際に接続する運用差分確認は公開直前の手元確認として扱う | 主要経路，呼び出し面，payload builder意味論，主要actionと管理系REST/raw RESTのIPC/REST基本契約，主要REST異常mapping，preflightの基本契約と主要なruntime判定は到達 |
| runtime | Gateway，REST，rate limit，snapshot，permission，audit，multi-shard supervisorを持つ | Gateway compression，ETF encoding，公式event全種の個別化は未完 | 非Voice Gateway型Botのruntimeとして公開候補 |
| 汎用到達性 | `D7RST`，`D7RBD`，`D7GWS`，payload builderでtyped API外の機能にも到達できる | `D7RST` はtyped APIのpermission preflightを迂回するため，信頼済みBot code向け低レベルAPIとして扱う．Discord仕様変更への追従と個別API化は継続作業 | 方針に合うが運用注意が必要 |
| 配布境界 | `Cargo.toml` include whitelistと `make package-list-check` で公開crateを固定する | clean環境での公開直前再検証が必要 | 方針に合う |

#### dfor-77 extension

| 領域 | 現状 | 未達成または弱い点 | 評価 |
| --- | --- | --- | --- |
| 定数include | `include/dfor77.inc` を配布し、内容は手動展開可能な `INTEGER` / `PARAMETER` 定義に保つ | なし。必須機能ではなく便利化として扱う必要がある | 実装済み |
| 拡張Bot検査 | 開発用test内で `INCLUDE 'dfor77.inc'` を使うBot断片をbuildし、wrapper挙動を検査する | これは公開crate外の開発検査であり、標準Bot到達性の根拠にはしない | 方針に合う |
| 便利wrapper | query，command，Auto Moderation，data URI，ファイル変換とメモリ変換を `extension/dfor77_extension.f` に配布する | 深いcommand schemaなどのbuilderは追加余地がある．標準側の `D7CMD`，`D7B64`，`D7URI` と実装を共有する | 実装済み |
| 拡張サンプル | `examples/sample_moderation/sample_moderation_extension.f` を `examples/sample_moderation/sample_moderation.f` の標準サンプルと並べてリポジトリに置く | 機能到達性の根拠にはしない | 最小実装 |

### REST endpoint棚卸し

確認日: 2026-07-03．公式Discord docsのHTTP API Resourcesには，Application Role Connection Metadata，Application，Audit Log，Auto Moderation，Channel，Emoji，Entitlement，Guild，Guild Scheduled Event，Guild Template，Invite，Lobby，Message，Poll，SKU，Soundboard，Stage Instance，Sticker，Subscription，User，Voice，Webhookがある．Bot libraryとしては，InteractionsのApplication CommandsとReceiving and Respondingも同じREST API棚卸しへ含める．

分類:

| 分類 | 意味 |
| --- | --- |
| `dfor-77必須` | Discord Botとして一般的に使われ、ANSI X3.9-1978に従うBotから専用 `D7` APIまたは十分に安定したstrict APIで使える必要がある |
| `dfor-77提供可能` | Bot tokenとRESTで扱えるが、一般的Botの中核ではない。`D7RST` 到達後に専用typed wrapperを順次追加する対象 |
| `extension向き` | `dfor-77` からraw JSON等で到達可能に保つが、可変長配列、深いJSON、file/data URI、query builder等の便利化は `dfor-77 extension` が適する |
| `困難` | OAuth2 user token、外部HTTP server、Social SDK、Voice音声本体などが主対象であり、`dfor-77` と `dfor-77 extension` のどちらでも通常Bot機能として提供しにくい |

現状:

| 表記 | 意味 |
| --- | --- |
| `専用D7 APIあり` | FORTRAN Botから公開 `D7` APIで呼べる |
| `D7RST可` | `D7RST` と `D7RBD` で呼べるが、専用typed wrapperはない．typed APIのpermission preflightを迂回するため，信頼済みBot code向け低レベルAPIとして扱う |
| `Rust内部のみ` | preflight等でRust runtime内部には経路があるが、FORTRAN公開APIではない |
| `未実装` | 専用経路なし．`D7RST` で届く場合も，個別検証，型付け，permission preflightはない |
| `対象外` | 現時点の非Voice Bot公開条件から外す、または通常Bot libraryの範囲外 |

#### dfor-77必須

| Resource | endpoint群 | 現状 | 未達成または次作業 |
| --- | --- | --- | --- |
| Message / Channel message | Create Message、file upload付きCreate Message、Get Channel Messages、Get Channel Message、Edit Message、Delete Message、Bulk Delete Messages | `D7SND`、`D7SJP`、`D7STK`、`D7POL`、`D7UPL`、`D7UPM`、`D7DEL`、`D7GMS`、`D7LMS`、`D7EMS`、`D7BMD` は専用D7 APIあり | response bodyは `D7RBD` で読む。editの添付差し替えや複雑payloadはextension builderの追加余地がある |
| Message reaction | Create Reaction，Delete Own/User Reaction，Delete All，Delete All for Emoji，Get Reactions | `D7RAC` は追加/削除系5mode，`D7GRU` はユーザー取得の専用D7 APIあり | `D7GRU` は通常／burst，`after` cursor，1〜100件の取得に対応する．絵文字を内部でURL encodeし，応答は `D7RBD` で読む |
| Message pin / crosspost | Crosspost Message、Get Channel Pins、Pin Message、Unpin Message、旧pin API | `D7PIN` はmodeでpins取得、pin、unpin、crosspostを扱う | 旧pin API差分はDiscord staging環境で確認する |
| Channel | Get Channel、Modify Channel、Delete/Close Channel、Set Voice Channel Status | `D7GCH`、`D7ECH`、`D7DCH`、`D7SVC` は専用D7 APIあり | Voice Channel StatusはREST管理操作として扱い、音声通信本体とは分ける |
| Channel permission / invite | Edit/Delete Channel Permissions、Get/Create Channel Invites、Follow Announcement Channel | `D7CPM`、`D7DPM`、`D7GIN`、`D7CIN`、`D7FOL` は専用D7 APIあり | invite target usersのCSV uploadはextension helper対象 |
| Thread | Start Thread from Message、Start Thread without Message、Start Thread in Forum/Media Channel、Join/Leave Thread、Add/Remove/Get/List Thread Members、List Archived Threads | `D7THR` はmodeで主要thread endpointを扱う | queryは `D7XQ0`/`D7XQA` で補助できる。forum/media thread payload builderは追加余地がある |
| Guild core | Get Guild、Modify Guild、Get/Create Guild Channels、Modify Channel Positions、List Active Guild Threads | `D7GGD`、`D7EGD`、`D7GCS`、`D7CCR`、`D7MCP`、`D7ATH` は専用D7 APIあり | guild作成/削除など危険度が高いendpointはD7RST経由で個別判断 |
| Guild member | Get/List/Search/Add/Modify/Remove Guild Member、Modify Current Member、Modify Current User Nick、timeout解除 | `D7GMB`、`D7LMB`、`D7SMB`、`D7AMB`、`D7EGM`、`D7KIK`、`D7ECM`、`D7TMO`、`D7UNT` は専用D7 APIあり | 大量list/searchのqueryとページングはextension helperで補助する |
| Guild role | Get/List/Create/Modify/Delete Role、Modify Role Positions、Add/Remove Member Role、role member counts | `D7LRO`、`D7CRO`、`D7ERO`、`D7DRO`、`D7RPS`、`D7AMR`、`D7RMR` は専用D7 APIあり。role member countsはD7RST可 | role member countsのread専用wrapperは追加余地がある |
| Guild ban | Get Guild Bans、Get Guild Ban、Create/Remove/Bulk Guild Ban | `D7LBN`、`D7GBN`、`D7BAN`、`D7UBN`、`D7BBN` は専用D7 APIあり | queryはextension helperで補助する |
| Audit Log | Get Guild Audit Log | `D7AUD` は専用D7 APIあり | response bodyは `D7RBD` で読む。queryはextension helperで補助する |
| Auto Moderation | List/Get/Create/Modify/Delete Auto Moderation Rule | `D7AUM` はmodeで主要endpointを扱う。`D7XAMK` はkeyword block rule payloadを補助する | trigger/actionsの全variant builderは追加余地がある |
| Application Commands | Global/Guild commandのGet/Create/Edit/Delete/Bulk Overwrite、command permissions | `D7ACR` はGlobal/Guild commandのCRUD/Bulk相当をmethodとpayloadで扱える。permissionsはBearer tokenが必要なendpointを含む | Bot tokenで扱えるcommand本体は概ね到達。permissions更新は通常Bot tokenだけでは完結しないため困難側に分離 |
| Interaction response | Create Interaction Response、Get/Edit/Delete Original Response、Create/Get/Edit/Delete Followup Message | `D7ICB` はinitial callback、`D7IFU` はfollowup/original response系の専用D7 API。現在処理中のinteractionでDiscordが `10062 Unknown interaction` を返し，かつ3秒を超過しているinitial callbackは `INTERACTION_DEADLINE_EXCEEDED` として判別する | attachment付きfollowup/editはextension helper追加余地がある。自動defer応答は未実装 |
| Webhook | Create Webhook、Get Channel/Guild Webhooks、Get/Modify/Delete Webhook、Execute Webhook、Get/Edit/Delete Webhook Message | `D7CWH`、`D7LWH`、`D7GWH`、`D7EWH`、`D7DWH`、`D7WHP`、`D7WHM` は専用D7 APIあり | token付き経路はsecret扱いが特殊なため、運用上のredaction確認を続ける |
| User bot-facing | Get Current User、Get User、Create DM | `D7GCU`、`D7GUR`、`D7CDM` は専用D7 APIあり | 大量DM防止の運用注意が必要 |
| Gateway helper REST | Get Gateway Bot | `D7GWB` は専用D7 APIあり | diagnostic/read-only用途 |

#### dfor-77提供可能

| Resource | endpoint群 | 現状 | 理由 |
| --- | --- | --- | --- |
| Application | Get Current Application、Edit Current Application、Get Application Activity Instance | D7RST可 | Bot運用やApp設定管理で有用。Editはimage dataやURL検証が絡むためextension builderがあるとよい |
| Application Role Connection Metadata | Get/Update metadata records | D7RST可 | Linked Roles向け。Bot tokenでmetadata定義は扱えるが、userごとのrole connection更新はOAuth2側 |
| Emoji | Guild/Application EmojiのList/Get/Create/Modify/Delete | D7RST可 | guild運用で有用。create/modifyはimage dataやrate limit差分があり、extension helper向き |
| Entitlement | List/Get/Consume/Create Test/Delete Test Entitlement | D7RST可 | monetization Botでは必要。一般Botの中核ではない |
| Guild Scheduled Event | List/Create/Get/Modify/Delete Event、Get Event Users | D7RST可 | community Botで有用。recurrenceやimageはextension builder向き |
| Guild Template | Get/Create/Sync/Modify/Delete Template | D7RST可 | guild管理Botで有用。一般Botの中核ではない |
| Invite | Get Invite、Delete Invite | D7RST可 | invite管理で有用。Target Users CSV系はextension向き |
| Poll | Get Answer Voters，End Poll | 作成は `D7POL`，回答別の投票者取得は `D7PAV`，終了は `D7EPL` の専用D7 APIあり．標準FORTRANとextensionの両方で利用できる | cursor付きページング，回答IDの非連番対応，終了結果不明時の非再送に対応する．vote自体はBot不可 |
| SKU | List SKUs | D7RST可 | monetization連携で有用。read-onlyでstrict提供しやすい |
| Soundboard | Send Soundboard Sound、List/Get/Create/Modify/Delete Guild Soundboard Sounds、List Default Sounds | D7RST可。`D7XF64` と `D7XURI` でbase64 data URI生成を補助できる | 音声接続本体ではないREST管理機能。createはsound data helperを使う |
| Stage Instance | Create/Get/Modify/Delete Stage Instance | D7RST可 | stage管理Botで有用。音声encode/decodeとは別のREST管理機能 |
| Sticker | Get Sticker、List Packs、Get Pack、Guild Sticker CRUD | messageで既存sticker送信は `D7STK` あり。Sticker resource CRUDはD7RST可 | guild運用で有用。createはfile uploadがextension向き |
| Subscription | List SKU Subscriptions、Get SKU Subscription | D7RST可 | monetization連携で有用。一般Botの中核ではない |
| Voice REST管理 | List Voice Regions、Get/Modify Voice State | `D7VST` はGateway voice state payload enqueue。REST voice stateはD7RST可 | stage/voice管理操作としては提供可能。音声通信本体は対象外 |

#### extension向き

| Resource | endpoint群 | 現状 | extension向きの理由 |
| --- | --- | --- | --- |
| Message rich payload | embeds、components、allowed mentions、attachments差し替え、poll emoji、thread starter message | payload builderの一部は `D7PEM`、`D7PBT`、`D7PAC`、`D7PSM`、`D7IMR` 等で専用D7 APIあり | strict APIは固定長引数と配列で到達させ、複雑なJSON構築はinclude定数や便利builderで補助するのがよい |
| Application command payload | options、choices、localization、contexts、integration types、autocomplete | `D7SCM`、`D7SCX`、`D7XACB` と `D7ACR` あり | command schemaは深い可変JSON。strict APIはraw payload到達を保ち、extensionでschema builderを増やす |
| Auto Moderation payload | trigger metadata、actions、exempt roles/channels | `D7AUM` と `D7XAMK` あり | 大きな可変配列とネストが多い。必須機能はD7AUM/raw payloadで保ち、extensionでbuilder化する |
| Emoji/Sticker/Soundboard create | image data、multipart、base64 data URI、sound data | D7RST可。一部message uploadは `D7UPL`/`D7UPM` あり。`D7XURI`、`D7XF64` でdata URIを補助する | binary/file/data URI変換はFORTRAN77だけでは扱いにくい。extension helperを置く価値が高い |
| Invite target users | CSV file upload、job status | D7RST可だが専用file upload経路なし | CSV生成とmultipartが必要。extension向き |
| Webhook token endpoints | token付きexecute、Slack/GitHub互換execute、webhook message操作 | D7RST可 | Bot tokenと別のsecret tokenを扱うため、秘匿とredactionを含む便利APIが必要 |
| Read endpoint pagination | before/after/limit、array query、複数同名query | D7RST可。`D7XQ0`、`D7XQA` でqueryを生成できる | `D7RST` のpath文字列で到達可能だが、query builderはextensionで安全化しやすい |

#### 困難または対象外

| Resource | endpoint群 | 現状 | 理由 |
| --- | --- | --- | --- |
| OAuth2 user-token API | authorize/token/revoke、user bearer前提のUser Connections、Current User Guild Member、Application Role Connection user更新、Application Command Permissions更新 | 未実装 | `dfor-77` のBot token runtimeだけでは完結しない。外部OAuth2 flowとuser consentが必要 |
| Lobby | Create/Join/Get/Modify/Delete Lobby、member操作、lobby message、channel linking、lobby invite | 一部Bot token endpointはD7RSTで届く可能性あり。専用APIなし | Social SDKやBearer token前提のendpointが多く、通常Bot libraryの中核ではない |
| Group DM | Group DM create、Group DM recipient add/remove | 未実装 | deprecated GameBridge SDKやuser access tokenに寄る。通常Bot機能として扱いにくい |
| Incoming interaction HTTP server | outgoing webhookでのinteraction受信、signature検証、HTTP response | 未実装 | Gateway `INTERACTION_CREATE` 受信とは別の公開HTTP server機能。runtimeに別server層が必要 |
| Voice audio本体 | Voice WebSocket、UDP/RTP、audio encode/decode、voice send/receive | 対象外 | 現在の非Voice Bot公開条件から外す。REST voice stateやGateway voice state enqueueとは別物 |

### 管理Bot機能の到達性

汎用管理用botが持つ機能を、dfor-77が提供するlibrary基盤で作れるかを分ける。dfor-77はBot製品ではなくruntime/API wrapperであるため、永続DB、dashboard、課金、UI、外部通知連携はBotアプリケーション側の責務とする。

| 管理Bot機能 | dfor-77での到達性 | 残るアプリケーション実装 |
| --- | --- | --- |
| message削除、timeout、kick、ban、bulk ban | `D7DEL`、`D7TMO`、`D7KIK`、`D7BAN`、`D7BBN` で可能 | ルール定義、違反回数、例外role/channel管理 |
| Auto Moderation rule管理 | `D7AUM` と `D7XAMK` でkeyword ruleは可能 | trigger/action全variantのbuilder、guild別設定UI |
| moderation log | `D7SND`、webhook API、audit logで可能 | 永続検索、case番号、dashboard |
| 反応ロール | `D7REA` / `D7RER` event、`D7EMJ`、`D7AMR`、`D7RMR` で可能 | 反応とroleの対応表、永続設定、復旧処理 |
| welcome / leave log | `D7MBA` / `D7MBR` event、`D7SND` で可能 | テンプレート、画像生成、guild別設定 |
| slash command設定 | `D7ACR`、`D7SCM`、`D7SCX`、`D7XACB` で可能 | command schemaの大規模生成、localization |
| ticket / suggestion / starboard | message、thread、reaction、role、webhook APIで構成可能 | DB、状態遷移、権限設計、UI |
| leveling / reputation | message eventで算出可能 | 永続DB、rate limit、rank card画像生成 |
| music / voice audio | 対象外 | Voice WebSocket、UDP/RTP、codec実装が別途必要 |

短期実装順:

| 優先 | 内容 |
| --- | --- |
| 1 | 完了。`D7GMS`、`D7LMS`、`D7GCH`、`D7GGD`、`D7GMB`、`D7LRO`、`D7AUD`、`D7AUM`、`D7CWH`、`D7IFU` などを追加した |
| 2 | 完了。`D7RST` 経由だった管理系を専用typed wrapperへ広げ、`D7RBD` と組み合わせるacceptanceを追加した |
| 3 | 完了。`extension/dfor77_extension.f` に `D7XQ0`、`D7XQA`、`D7XACB`、`D7XAMK`、`D7XURI`、`D7XF64` を追加した。管理Bot基盤として reaction、member、message/channel/thread/guild/role更新eventも追加した |
| 4 | OAuth2、Lobby、incoming interaction HTTP server、Voice audioは別設計として扱い、v0.3.0の必須条件から分離する |

### 要件2の作業条件

ANSI X3.9-1978準拠を必須とするFORTRAN配布物は、配布対象の `fortran/` と標準サンプルに置く。`include/dfor77.inc` は `dfor-77 extension` 配布物として別に扱う。公開crateには開発用 `tests/` を含めない。ANSI X3.9-1978準拠を必須とするFORTRAN配布物は次を満たす必要がある。

| 条件 | 対応 |
| --- | --- |
| 固定形式 | 72桁以内、tabなし、継続行は固定形式 |
| 名前 | Botから見える名前とwrapper内補助名を英数字6文字以内に保つ |
| 非標準構文排除 | `INCLUDE`、underscore、`::`、`DO WHILE`、`SELECT`、module/use構文を使わない |
| 非標準I/O排除 | `FORMAT(I0)`、`CALL FLUSH` に依存しない |
| 検証 | `make source-policy` と `make fortran-wrapper` をrelease gateに含める |
| 配布境界 | `make package-list-check` で開発用tests、CI、生成物がcrateへ入らないことを固定する |

現状では、配布対象FORTRAN source、手動展開可能な `dfor77.inc`、README内FORTRAN例を `make source-policy` で検査する。より厳密な達成確認には、gfortran以外のFORTRAN77検査器、またはANSI X3.9-1978構文に特化した静的検査をCIへ追加する必要がある。

### dfor-77 extension の作業条件

`dfor-77 extension` は、プログラム作成を容易または柔軟にするため、`dfor77.inc`、便利wrapper、拡張サンプル等を提供できる。`INCLUDE` 文、MIL-STD-1753相当の一般的拡張、gfortran等の実用環境に依存するものは `dfor-77 extension` として明示する。`dfor-77 extension` にのみ存在する必須機能は設けない。

`dfor77.inc` は `dfor-77 extension` の配布物である。ただし、その内容はFORTRAN77標準環境で手動展開して利用できる `INTEGER` / `PARAMETER` 定義として提供する。

### 要件3の作業条件

ANSI X3.9-1978に従うBotがこのプログラム上で提供可能なDiscord機能を使うには、すべてのruntime機能に対して、`INCLUDE` や拡張構文を必要としない `D7` APIを提供する必要がある。

| 条件 | 対応 |
| --- | --- |
| 呼び出し面 | すべての機能を6文字以内のSUBROUTINE/FUNCTIONとINTEGER/CHARACTER引数で表す |
| 定数 | `dfor77.inc` と同じ値をREADMEへ明記し、標準BotがPARAMETERとして複写できるようにする |
| 機能到達性 | typed APIがない機能も `D7RST`、`D7GWS`、payload builderで到達できるようにする |
| 文字列契約 | NUL終端、可変長型、module、derived type、optional引数へ依存しない |
| 受け入れ検証 | INCLUDEなしの標準Botで主要APIカテゴリを横断して使うacceptance testを持つ |
| dfor-77 extensionとの分離 | `dfor77.inc`、便利wrapper、拡張サンプルは `dfor-77 extension` として扱い、機能到達性の根拠にしない |

v0.3.0では、公開API名、主要action、主要APIカテゴリのINCLUDEなし到達性はこの方針に沿う。payload builderはINCLUDEなしの標準BotでJSON生成の意味論も検査し、message削除、timeout、通常送信、JSON payload送信、sticker、poll、reaction、単一/複数添付、interaction callback、application command、typing、管理系REST wrapper全13種、raw REST、response body取得はINCLUDEなしの標準BotでIPC契約とmock REST副作用を検査する。REST異常系はinvalid form body、Discord codeなし400、missing access、missing permission、channel/guild/member/message/user not found、system message、invalid message type、archived/locked thread、forum parent、unsupported channel type、HTTP 429、Discord slowmode/write rate limit code、送信系5xx、read-only 5xxのstatus mappingと `D7ERR` で読めるHTTP/Discord code、およびaudit上の意味分類をINCLUDEなしの標準Botで検査する。preflight系は出力契約、エラー時の `CAN` 値、必須IDのローカル拒否、runtime snapshot上のsend/delete/timeout許可と拒否、forum parent channel、archived/locked public threadのsend拒否をINCLUDEなしの標準Botで検査する。Discord公式の全JSON error codeを個別typed APIごとに固定する段階ではないため、新たに運用で必要になったerror codeはDiscord staging環境で観測してから追加する。

### 開発用ファイルの扱い

開発用の統合テストやfixtureは `tests/` に置き、公開crateには含めない。利用者向けサンプルとしてリポジトリに置くものは `examples/sample_moderation/` に分離し、公開crateには含めない。

`tests/` 内の一部FORTRAN snippetは、`INCLUDE` などの `dfor-77 extension` 利用も検査する。これは拡張FORTRAN77環境向けの試験であり、配布対象FORTRAN77プログラムの厳密性や `dfor-77` の機能到達性を判断する根拠にはしない。

### 開発環境の更新

開発には現在のstable Rustと，rustfmt，Clippy，gfortran，makeを使用する．最低対応Rustは1.85.0で，開発用のstableとは別に検証する．依存関係の監査にはCIと同じcargo-audit 0.22.2を使用する．

```sh
rustup update stable
rustup component add rustfmt clippy --toolchain stable
rustup toolchain install 1.85.0 --profile minimal
cargo +stable install cargo-audit --locked --version 0.22.2
```

`Cargo.lock` はリポジトリと公開crateに含める．依存関係を更新した場合は，`make msrv-check` と `make release-check` で最低対応Rust，ビルド，テスト，監査，配布物を確認する．

### 開発ゲート

通常の確認:

```sh
make check
```

開発用integration testまで含める確認:

```sh
make dev-check
```

公開前確認:

```sh
make release-check
```

crates.io dry-runまで含める確認:

```sh
make publish-check
```

CI相当:

```sh
make ci-check
```

主な個別ゲート:

| Gate | 内容 |
| --- | --- |
| `make source-policy` | Rust source policy、配布FORTRAN/README FORTRAN例のFORTRAN77方針検査、README固定目的・用語・文体検査、package include list整合検査 |
| `make dev-check` | `tests/` 内の開発用integration testを含む確認 |
| `make package-list-check` | 公開crateに含めるファイルの固定 |
| `make config-check` | 同梱config sampleのparseと安全側既定値 |
| `make license-check` | 依存crate license/source policy確認 |
| `make advisory-check` | `cargo audit --deny warnings` |
| `make staging-rehearsal` | Discordと接続する前のmock受け入れ確認 |
| `make staging-live-preflight` | staging token/hash key、Bot binary、config、session path確認 |
| `make staging-live-run` | preflight後にDiscord staging Botを起動 |
| `make staging-live-24h` | preflight後にDiscord staging Botを指定秒数起動し、終了後にaudit logを検査 |
| `make staging-audit-check` | Discord staging環境で接続した後のaudit log検査 |

直近のローカル確認（2026-07-04）では、`make release-check` と `make publish-check` が成功した。これにより、format、whitespace、コメント方針、Rust unit/integration、FORTRAN sample、FORTRAN extension sample、FORTRAN acceptance、staging rehearsal、config sample、source policy、clippy、docs、license policy、`cargo audit --deny warnings`、package file list、`cargo package`、`cargo publish --dry-run` が通っている。

`cargo package` と `cargo publish --dry-run` は、開発用integration testが公開crateに含まれない旨の警告を出す。この警告は、`tests/` を公開crateから除外する方針による既知の出力である。公開対象fileは `make package-list-check` と source policyで固定する。

公開crateに含まれる `make check` は、開発用 `tests/` へ依存しない基本検証である。`make dev-check`、`make release-check`、`make publish-check` はrepository checkout向けの開発gateであり、公開crateから除外する `tests/` を使う。

### 公開前確認

公開用リポジトリには，ライブラリ本体，extension，公開サンプル，テスト，ドキュメントのみを置く．実際に運用するBot，デプロイ設定，認証情報，運用ログ，検証記録は，このGitリポジトリの外で管理する．

- `dev` でREADME，CI，実装，テストの整合性を確認する
- Discordに接続する運用確認は，必要に応じてリポジトリ外の非公開環境で行う．ライブラリのローカル変更を使う場合は，その差分も確認する
- 追跡ファイルと公開するコミットに，実値，secret，運用ログ，非公開設定，生成物が含まれていないことを確認し，`make publish-check` を通す
- 公開先ブランチへの反映，コミット，pushは，対象の差分と履歴を確認してから明示的に行う

### 今後の方針

#### dfor-77

| 優先度 | 項目 |
| --- | --- |
| 中 | 全REST endpointの個別typed wrapper化を進める |
| 中 | 既知Gateway state eventの個別FORTRAN event化を増やす |
| 中 | Gateway zlib-stream compression / ETF encoding |
| 低 | `src/runtime.rs` と `src/gateway.rs` の保守分割 |

Voice WebSocket，UDP/RTP，音声encode/decode，voice send/receiveは当面実装しない．これらは音声系機能であり，現在の公開準備における必須条件に含めない．

#### dfor-77 extension

| 優先度 | 項目 |
| --- | --- |
| 中 | `include/dfor77.inc` の使い方を利用者向けに保ちつつ，標準Botでは不要であることを明確にし続ける |
| 低 | 便利wrapperを追加する場合は，同じ機能へ標準 `D7` APIからも到達できることを先に確認する |

### builderとmessage更新の回帰検証

`tests/fortran_builders.rs` は，72桁固定形式，最適化と実行時境界検査を有効にして公開wrapperをコンパイルする．標準側のバイナリ変換，data URI，context menu生成はextensionをlinkせず検査する．全256バイト値，空入力，padding，UTF-8文字数，25 option，容量0，切り詰め，内部上限，queryの同名key，percent encoding，バッファ超過エラーの保持，使用中unitの保全を検査する．通常の `cargo test` と `make dev-check` に含まれる．

GatewayとRESTの添付画像判定は `src/snapshot.rs` の共通処理を使う．`MessageSnapshotPatch.image_attachment_count` は，添付情報の省略を `None`，明示的な削除を `Some(0)` として区別する．Gatewayの部分更新とruntimeのmessage検索まで回帰検証する．
