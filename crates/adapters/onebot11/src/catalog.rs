//! 平台方法表：这台 QQ 号声明给模型看的**全部**能力。
//!
//! 目标是一句话：**人在一个房间里能做的事，模型都能做**。所以这里不是「挑几个安全的」，
//! 而是把 OneBot 11（含 go-cqhttp / NapCat / LLOneBot 的常用扩展）里跟「聊天」有关的
//! 动作全列上；不安全的那些不删，只标 `Risk::Destructive`，由配置决定放不放行
//! （`[[connections]] allow_destructive`）。
//!
//! 故意不列的东西，理由都是同一条——**那不是「人能做的事」**：
//! - `get_cookies` / `get_csrf_token` / `get_credentials`：那是账号凭据，不是聊天能力。
//! - `set_restart` / `clean_cache`：那是机器人进程运维，模型碰它只会把桥搞挂。
//! - `get_status` 之外的探活类接口价值低，占了工具目录的位置不划算。
//!
//! core 不认识这里任何一个名字，它只认 `MethodSpec` 这个形状（ADR-0020）。

use acb_kernel::{MethodSpec, ParamSpec, ParamType, Risk, outbound::tags};

// ── 参数写法 ──────────────────────────────────────────────────────────────
//
// 一张表几百行，字段名全写出来会淹掉内容，所以统一用这几个短构造。

/// 必填字符串。
const fn req(name: &'static str, desc: &'static str) -> ParamSpec {
    ParamSpec::required(name, desc)
}

/// 可选字符串。
const fn opt(name: &'static str, desc: &'static str) -> ParamSpec {
    ParamSpec::optional(name, ParamType::String, desc)
}

/// 可选数字。
const fn num(name: &'static str, desc: &'static str) -> ParamSpec {
    ParamSpec::optional(name, ParamType::Integer, desc)
}

/// 可选布尔。
const fn flag(name: &'static str, desc: &'static str) -> ParamSpec {
    ParamSpec::optional(name, ParamType::Bool, desc)
}

/// 必填但形状不固定（消息段：字符串或数组都合法）。
const fn any(name: &'static str, desc: &'static str) -> ParamSpec {
    ParamSpec::any(name, desc)
}

/// 可选、形状不固定。
const fn maybe_any(name: &'static str, desc: &'static str) -> ParamSpec {
    ParamSpec::maybe_any(name, desc)
}

const fn m(
    name: &'static str,
    summary: &'static str,
    tags: &'static [&'static str],
    risk: Risk,
    params: &'static [ParamSpec],
) -> MethodSpec {
    MethodSpec {
        name,
        summary,
        tags,
        risk,
        params,
    }
}

// ── 方法表 ────────────────────────────────────────────────────────────────
//
// 命名是 `归属.动作`：归属跟 OneBot 的命名空间对齐（message / group / friend / account…），
// 方便模型看到消息里的 `{"type":"forward"}` 就联想到 `message.forward_content`；
// tags 是跨归属的用途标记，模型按需求找入口时靠它（`tag=image` 一次捞齐发图、取图、改群头像）。

pub static METHODS: &[MethodSpec] = &[
    // ── 账号自己 ──
    m(
        "account.login_info",
        "这台号登的是谁：QQ 号、昵称",
        &[tags::ACCOUNT],
        Risk::Read,
        &[],
    ),
    m(
        "account.status",
        "账号在线状态：是否在线、是否正常",
        &[tags::ACCOUNT, tags::SYSTEM],
        Risk::Read,
        &[],
    ),
    m(
        "account.version",
        "平台实现的版本信息。排查「这个方法到底支不支持」时有用",
        &[tags::SYSTEM],
        Risk::Read,
        &[],
    ),
    // ── 好友 ──
    m(
        "friend.list",
        "好友列表：QQ 号、昵称、备注",
        &[tags::FRIEND, tags::MEMBER],
        Risk::Read,
        &[],
    ),
    m(
        "friend.get_info",
        "查一个陌生人/好友的资料：昵称、性别、年龄、签名",
        &[tags::FRIEND, tags::MEMBER],
        Risk::Read,
        &[req("user_id", "QQ 号")],
    ),
    m(
        "friend.approve",
        "同意或拒绝一个加好友请求。flag 从好友请求事件里拿",
        &[tags::FRIEND, tags::APPROVE],
        Risk::Write,
        &[
            req("flag", "请求的 flag，来自好友请求事件"),
            ParamSpec::optional("approve", ParamType::Bool, "true 同意，false 拒绝"),
            opt("remark", "同意时给他设的备注名"),
        ],
    ),
    m(
        "friend.delete",
        "删好友。删掉之后对方从列表里消失",
        &[tags::FRIEND, tags::ADMIN],
        Risk::Destructive,
        &[req("user_id", "要删的 QQ 号")],
    ),
    // ── 群：看 ──
    m(
        "group.list",
        "这台号在哪些群里：群号、群名。想给别的群发东西时先用它拿群号",
        &[tags::GROUP, tags::MEMBER],
        Risk::Read,
        &[],
    ),
    m(
        "group.get_info",
        "查一个群的资料：群名、人数上限、当前人数、群主",
        &[tags::GROUP],
        Risk::Read,
        &[opt("group_id", "群号；在群里调用可以不填")],
    ),
    m(
        "group.get_member_info",
        "查一个群成员的昵称、群名片、角色（群主/管理员/成员）、入群时间",
        &[tags::MEMBER, tags::GROUP],
        Risk::Read,
        &[
            req("user_id", "成员号"),
            opt("group_id", "群号；在群里调用可以不填"),
        ],
    ),
    m(
        "group.get_member_list",
        "列出群成员。大群会很长，慎用",
        &[tags::MEMBER, tags::GROUP],
        Risk::Read,
        &[opt("group_id", "群号；在群里调用可以不填")],
    ),
    m(
        "group.get_honor",
        "群荣誉：龙王、群聊之火、活跃天数榜之类。type 可填 all/talkative/performer/legend",
        &[tags::GROUP, tags::MEMBER],
        Risk::Read,
        &[
            opt("group_id", "群号；在群里调用可以不填"),
            opt("type", "荣誉类型，默认 all"),
        ],
    ),
    m(
        "group.get_system_msg",
        "群系统消息：谁申请加群、谁被邀请进群、哪些还没处理（含处理用的 flag）",
        &[tags::GROUP, tags::APPROVE],
        Risk::Read,
        &[],
    ),
    m(
        "group.get_notice",
        "读群公告：内容、发布者、发布时间",
        &[tags::GROUP, tags::MESSAGE],
        Risk::Read,
        &[opt("group_id", "群号；在群里调用可以不填")],
    ),
    m(
        "group.files",
        "列出群文件的根目录：文件名、大小、file_id、busid（下载要这后两个）",
        &[tags::GROUP, tags::FILE],
        Risk::Read,
        &[opt("group_id", "群号；在群里调用可以不填")],
    ),
    m(
        "group.files_in_folder",
        "列出群文件某个文件夹里的东西",
        &[tags::GROUP, tags::FILE],
        Risk::Read,
        &[
            opt("group_id", "群号；在群里调用可以不填"),
            req("folder_id", "文件夹 id，从 group.files 拿"),
        ],
    ),
    m(
        "group.file_url",
        "拿群文件的下载地址（会过期，要用的时候现取）",
        &[tags::GROUP, tags::FILE, tags::DOWNLOAD],
        Risk::Read,
        &[
            opt("group_id", "群号；在群里调用可以不填"),
            req("file_id", "文件 id"),
            num("busid", "业务 id，通常 102；从 group.files 拿"),
        ],
    ),
    m(
        "group.file_system_info",
        "群文件系统概况：文件总数、已用空间",
        &[tags::GROUP, tags::FILE],
        Risk::Read,
        &[opt("group_id", "群号；在群里调用可以不填")],
    ),
    m(
        "group.essence_list",
        "群精华消息列表",
        &[tags::GROUP, tags::MESSAGE],
        Risk::Read,
        &[opt("group_id", "群号；在群里调用可以不填")],
    ),
    m(
        "group.at_all_remain",
        "还剩几次 @全体成员 能用（问的是这台号自己）",
        &[tags::GROUP, tags::ADMIN],
        Risk::Read,
        &[opt("group_id", "群号；在群里调用可以不填")],
    ),
    // ── 群：写 ──
    m(
        "group.send_notice",
        "发群公告。全群都会看到，属于管理员操作",
        &[tags::GROUP, tags::SEND, tags::ADMIN],
        Risk::Destructive,
        &[
            opt("group_id", "群号；在群里调用可以不填"),
            req("content", "公告正文"),
        ],
    ),
    m(
        "group.set_card",
        "改群名片。不填 user_id 就是改自己的；改别人的要管理员",
        &[tags::GROUP, tags::MEMBER],
        Risk::Write,
        &[
            opt("group_id", "群号；在群里调用可以不填"),
            opt("user_id", "改谁的；不填改自己"),
            opt("card", "新名片；空字符串 = 清空"),
        ],
    ),
    m(
        "group.set_remark",
        "给群设一个只有自己看得见的备注名",
        &[tags::GROUP],
        Risk::Write,
        &[
            opt("group_id", "群号；在群里调用可以不填"),
            req("remark", "备注名"),
        ],
    ),
    m(
        "group.upload_file",
        "往群文件里上传一个文件。file 可以是本地路径、http 地址或 base64://",
        &[tags::GROUP, tags::FILE, tags::UPLOAD],
        Risk::Write,
        &[
            opt("group_id", "群号；在群里调用可以不填"),
            req("file", "本地路径 / http 地址 / base64://"),
            req("name", "在群里显示的文件名"),
            opt("folder", "放进哪个文件夹（文件夹 id）"),
        ],
    ),
    m(
        "group.set_essence",
        "把一条消息设成群精华",
        &[tags::GROUP, tags::MESSAGE],
        Risk::Write,
        &[req("message_id", "要设成精华的消息 id")],
    ),
    m(
        "group.delete_essence",
        "取消一条消息的群精华",
        &[tags::GROUP, tags::MESSAGE],
        Risk::Write,
        &[req("message_id", "要取消精华的消息 id")],
    ),
    m(
        "group.sign",
        "群签到（有些群开了这个玩法）",
        &[tags::GROUP],
        Risk::Write,
        &[opt("group_id", "群号；在群里调用可以不填")],
    ),
    m(
        "group.approve",
        "同意或拒绝一个加群/邀请请求。flag 和 sub_type 从 group.get_system_msg 或入群请求事件里拿",
        &[tags::GROUP, tags::APPROVE],
        Risk::Write,
        &[
            req("flag", "请求的 flag"),
            opt("sub_type", "add 或 invite，来自请求事件"),
            ParamSpec::optional("approve", ParamType::Bool, "true 同意，false 拒绝"),
            opt("reason", "拒绝理由"),
        ],
    ),
    // ── 群：管理（破坏性） ──
    m(
        "group.kick",
        "把一个人踢出群。他会被移出，之后能不能再加看 reject_add_request",
        &[tags::GROUP, tags::MEMBER, tags::ADMIN],
        Risk::Destructive,
        &[
            opt("group_id", "群号；在群里调用可以不填"),
            req("user_id", "踢谁"),
            flag("reject_add_request", "true = 顺便拒绝他再次加群"),
        ],
    ),
    m(
        "group.ban",
        "禁言一个人。duration 是秒，0 = 解除禁言",
        &[tags::GROUP, tags::MEMBER, tags::MUTE, tags::ADMIN],
        Risk::Destructive,
        &[
            opt("group_id", "群号；在群里调用可以不填"),
            req("user_id", "禁言谁"),
            req("duration", "秒；0 = 解除"),
        ],
    ),
    m(
        "group.whole_ban",
        "全员禁言开关。开了之后只有管理员能说话",
        &[tags::GROUP, tags::MUTE, tags::ADMIN],
        Risk::Destructive,
        &[
            opt("group_id", "群号；在群里调用可以不填"),
            ParamSpec::optional("enable", ParamType::Bool, "true 开，false 关"),
        ],
    ),
    m(
        "group.set_admin",
        "设/撤群管理员",
        &[tags::GROUP, tags::MEMBER, tags::ADMIN],
        Risk::Destructive,
        &[
            opt("group_id", "群号；在群里调用可以不填"),
            req("user_id", "谁"),
            ParamSpec::optional("enable", ParamType::Bool, "true 设，false 撤"),
        ],
    ),
    m(
        "group.set_name",
        "改群名。全群可见",
        &[tags::GROUP, tags::ADMIN],
        Risk::Destructive,
        &[
            opt("group_id", "群号；在群里调用可以不填"),
            req("group_name", "新群名"),
        ],
    ),
    m(
        "group.set_special_title",
        "设/撤群成员的专属头衔（群主才能设）",
        &[tags::GROUP, tags::MEMBER, tags::ADMIN],
        Risk::Destructive,
        &[
            opt("group_id", "群号；在群里调用可以不填"),
            req("user_id", "谁"),
            opt("special_title", "头衔；空 = 撤销"),
        ],
    ),
    m(
        "group.set_portrait",
        "换群头像。file 可以是本地路径、http 地址或 base64://",
        &[tags::GROUP, tags::ADMIN, tags::IMAGE],
        Risk::Destructive,
        &[
            opt("group_id", "群号；在群里调用可以不填"),
            req("file", "图片：本地路径 / http 地址 / base64://"),
        ],
    ),
    m(
        "group.leave",
        "退出群。is_dismiss 只有群主能用来解散",
        &[tags::GROUP, tags::ADMIN],
        Risk::Destructive,
        &[
            opt("group_id", "群号；在群里调用可以不填"),
            flag("is_dismiss", "true = 解散（仅群主）"),
        ],
    ),
    m(
        "group.delete_file",
        "删群文件。搞不好是别人传的，删了就没了",
        &[tags::GROUP, tags::FILE],
        Risk::Destructive,
        &[
            opt("group_id", "群号；在群里调用可以不填"),
            req("file_id", "文件 id"),
            num("busid", "业务 id，通常 102"),
        ],
    ),
    // ── 消息 ──
    m(
        "message.send_group",
        "往群里发消息。message 是消息段数组，文本、图片、语音、视频、文件、@ 都走这一个；\
         例：[{\"type\":\"text\",\"text\":\"看这个\"},{\"type\":\"image\",\"file\":\"https://…\"}]。\
         配合 message.send_forward 还能发合并转发",
        &[
            tags::SEND,
            tags::MESSAGE,
            tags::MEDIA,
            tags::IMAGE,
            tags::VOICE,
            tags::VIDEO,
            tags::FILE,
        ],
        Risk::Write,
        &[
            opt("group_id", "群号；在群里调用可以不填"),
            any("message", "消息段数组，如 [{\"type\":\"text\",\"text\":\"hi\"}]"),
        ],
    ),
    m(
        "message.send_private",
        "往一个 QQ 号发私聊消息。message 形状同 message.send_group",
        &[
            tags::SEND,
            tags::MESSAGE,
            tags::MEDIA,
            tags::IMAGE,
            tags::VOICE,
            tags::VIDEO,
            tags::FILE,
        ],
        Risk::Write,
        &[
            opt("user_id", "发给谁；在私聊里调用可以不填"),
            any("message", "消息段数组"),
        ],
    ),
    m(
        "message.send_forward",
        "发合并转发（那种「聊天记录」卡片）。messages 是节点数组：\
         [{\"type\":\"node\",\"data\":{\"name\":\"昵称\",\"uin\":\"QQ号\",\"content\":[消息段]}}]",
        &[tags::SEND, tags::MESSAGE, tags::FORWARD],
        Risk::Write,
        &[
            any("messages", "转发节点数组"),
            opt("group_id", "发到哪个群；在群里调用可以不填"),
            opt("user_id", "发到哪个私聊"),
        ],
    ),
    m(
        "message.get",
        "按消息 id 取一条消息的原始内容（含发送者、消息段）",
        &[tags::MESSAGE, tags::HISTORY],
        Risk::Read,
        &[req("message_id", "平台消息 id，就是消息里 quote 段的 message_id")],
    ),
    m(
        "message.recall",
        "撤回一条消息。撤别人的要管理员，撤自己的随时可以",
        &[tags::MESSAGE, tags::RECALL],
        Risk::Destructive,
        &[req("message_id", "要撤回的消息 id")],
    ),
    m(
        "message.history_group",
        "翻群消息历史。message_seq 是起点序号，往前取 count 条",
        &[tags::MESSAGE, tags::HISTORY, tags::GROUP],
        Risk::Read,
        &[
            opt("group_id", "群号；在群里调用可以不填"),
            num("message_seq", "从这条的序号开始往前"),
            num("count", "取多少条，默认 20"),
        ],
    ),
    m(
        "message.history_private",
        "翻和某个人的私聊历史",
        &[tags::MESSAGE, tags::HISTORY, tags::FRIEND],
        Risk::Read,
        &[
            opt("user_id", "和谁；在私聊里调用可以不填"),
            num("message_seq", "从这条的序号开始往前"),
            num("count", "取多少条，默认 20"),
        ],
    ),
    m(
        "message.forward_content",
        "读一条合并转发的正文。消息里只有一个转发 id 时用它展开",
        &[tags::FORWARD, tags::MESSAGE],
        Risk::Read,
        &[req("id", "转发 id，就是消息里 forward 段的 data.id")],
    ),
    m(
        "message.image",
        "拿到一个图片段的真实文件：本地路径或下载地址",
        &[tags::IMAGE, tags::MEDIA, tags::DOWNLOAD],
        Risk::Read,
        &[req("file", "图片段的 file 字段")],
    ),
    m(
        "message.voice",
        "把语音转成能用的格式，拿到文件路径。out_format 可填 mp3/amr/wav/silk",
        &[tags::VOICE, tags::MEDIA, tags::DOWNLOAD],
        Risk::Read,
        &[
            req("file", "语音段的 file 字段"),
            opt("out_format", "输出格式，默认 mp3"),
        ],
    ),
    m(
        "message.react",
        "给一条消息加表情回应（那个小表情）。emoji_id 是平台的表情 id",
        &[tags::MESSAGE, tags::IMAGE],
        Risk::Write,
        &[
            req("message_id", "哪条消息"),
            req("emoji_id", "表情 id"),
            flag("set", "true 加，false 取消"),
        ],
    ),
    m(
        "message.mark_read",
        "把消息标成已读",
        &[tags::MESSAGE],
        Risk::Write,
        &[req("message_id", "标到哪条")],
    ),
    // ── 互动 ──
    m(
        "poke.send",
        "拍一拍。群里要指定拍谁；私聊里不填就是拍对方",
        &[tags::POKE, tags::SEND, tags::MEMBER],
        Risk::Write,
        &[
            req("user_id", "拍谁；在私聊里调用可以不填"),
            opt("group_id", "在哪个群拍；在群里调用可以不填"),
        ],
    ),
    m(
        "like.send",
        "给一个人的资料卡点赞（那个「赞」）。times 最多 10",
        &[tags::MEMBER, tags::SEND],
        Risk::Write,
        &[req("user_id", "给谁点赞"), num("times", "点几次，默认 1，最多 10")],
    ),
    // ── 其它 ──
    m(
        "system.check_url",
        "问平台一个链接安不安全（腾讯自家的安全接口）。拿不准某个链接时先问一下",
        &[tags::SYSTEM],
        Risk::Read,
        &[req("url", "要检查的链接")],
    ),
];

// ── 路由 ──────────────────────────────────────────────────────────────────
//
// 方法名 → OneBot 动作。`FROM_GROUP` / `FROM_PRIVATE` 说这个参数能从房间推出来：
// 在群里调就不用重复填群号，在私聊里调就不用填对方 QQ。

/// 调哪个动作。
pub enum Target {
    /// 一个方法对一个动作。
    Fixed(&'static str),
    /// 群聊一个、私聊一个（拍一拍、发消息这两类平台分成两个接口）。
    ByRoom {
        group: &'static str,
        private: &'static str,
    },
}

/// 一条路由：方法名 → 平台动作。
pub struct Route {
    pub method: &'static str,
    pub target: Target,
    /// 在群房间里，这个参数缺了就用房间的群号补。
    pub from_group: Option<&'static str>,
    /// 在私聊房间里，这个参数缺了就用房间的对方号补。
    pub from_private: Option<&'static str>,
}

const fn route(method: &'static str, action: &'static str) -> Route {
    Route {
        method,
        target: Target::Fixed(action),
        from_group: None,
        from_private: None,
    }
}

/// 群参数 `group_id` 由房间补上。
const fn group_route(method: &'static str, action: &'static str) -> Route {
    Route {
        method,
        target: Target::Fixed(action),
        from_group: Some("group_id"),
        from_private: None,
    }
}

/// 私聊参数 `user_id` 由房间补上。
const fn private_route(method: &'static str, action: &'static str) -> Route {
    Route {
        method,
        target: Target::Fixed(action),
        from_group: None,
        from_private: Some("user_id"),
    }
}

pub static ROUTES: &[Route] = &[
    route("account.login_info", "get_login_info"),
    route("account.status", "get_status"),
    route("account.version", "get_version_info"),
    route("friend.list", "get_friend_list"),
    route("friend.get_info", "get_stranger_info"),
    route("friend.approve", "set_friend_add_request"),
    route("friend.delete", "delete_friend"),
    route("group.list", "get_group_list"),
    group_route("group.get_info", "get_group_info"),
    group_route("group.get_member_info", "get_group_member_info"),
    group_route("group.get_member_list", "get_group_member_list"),
    group_route("group.get_honor", "get_group_honor_info"),
    route("group.get_system_msg", "get_group_system_msg"),
    group_route("group.get_notice", "_get_group_notice"),
    group_route("group.files", "get_group_root_files"),
    group_route("group.files_in_folder", "get_group_files_by_folder"),
    group_route("group.file_url", "get_group_file_url"),
    group_route("group.file_system_info", "get_group_file_system_info"),
    group_route("group.essence_list", "get_essence_msg_list"),
    group_route("group.at_all_remain", "get_group_at_all_remain"),
    group_route("group.send_notice", "_send_group_notice"),
    group_route("group.set_card", "set_group_card"),
    group_route("group.set_remark", "set_group_remark"),
    group_route("group.upload_file", "upload_group_file"),
    route("group.set_essence", "set_essence_msg"),
    route("group.delete_essence", "delete_essence_msg"),
    group_route("group.sign", "set_group_sign"),
    route("group.approve", "set_group_add_request"),
    group_route("group.kick", "set_group_kick"),
    group_route("group.ban", "set_group_ban"),
    group_route("group.whole_ban", "set_group_whole_ban"),
    group_route("group.set_admin", "set_group_admin"),
    group_route("group.set_name", "set_group_name"),
    group_route("group.set_special_title", "set_group_special_title"),
    group_route("group.set_portrait", "set_group_portrait"),
    group_route("group.leave", "set_group_leave"),
    group_route("group.delete_file", "delete_group_file"),
    group_route("message.send_group", "send_group_msg"),
    private_route("message.send_private", "send_private_msg"),
    route("message.send_forward", "send_forward_msg"),
    route("message.get", "get_msg"),
    route("message.recall", "delete_msg"),
    group_route("message.history_group", "get_group_msg_history"),
    private_route("message.history_private", "get_friend_msg_history"),
    route("message.forward_content", "get_forward_msg"),
    route("message.image", "get_image"),
    route("message.voice", "get_record"),
    route("message.react", "set_msg_emoji_like"),
    route("message.mark_read", "mark_msg_as_read"),
    Route {
        method: "poke.send",
        target: Target::ByRoom {
            group: "group_poke",
            private: "friend_poke",
        },
        from_group: Some("group_id"),
        from_private: Some("user_id"),
    },
    route("like.send", "send_like"),
    route("system.check_url", "check_url_safely"),
];
