//! 出站：桥把编好的内容元素交给平台连接。
//!
//! 桥不碰平台细节，只知道「往这个房间，发这些元素」。怎么编成 OneBot 消息链或 Discord
//! 消息，是适配器的事。
//!
//! 分发按**连接**，不按平台：同一个平台可以挂多条连接（两个 QQ 号），房间的第一段就是
//! 连接名，查表不会撞。

use acb_proto::{Element, RoomId};
use serde_json::Value;
use thiserror::Error;

/// 一条已经发出去的消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    /// 平台侧消息 id。写进流水，方便引用。
    pub platform_id: String,
    /// 平台侧时间（Unix 毫秒）。平台不给就用本地时间。
    pub at: i64,
}

/// 出站失败。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OutboundError {
    /// 平台明确拒收（被踢了、没权限、内容违规）。
    #[error("平台拒收：{0}")]
    Rejected(String),
    /// 适配器编不出来（比如平台不支持这种元素，也降级不了）。
    #[error("平台不支持这种内容：{0}")]
    Unsupported(String),
    /// 通道本身没通（连接断了、账号掉线）。
    #[error("发送通道不可用：{0}")]
    Unavailable(String),
}

/// 一个平台方法的参数。
///
/// 内部用这个简化形状，**对外渲染成 JSON Schema** 给模型看（见 ADR-0020）：
/// 适配器作者不用碰 schema 库，模型看到的还是它最熟的形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParamSpec {
    pub name: &'static str,
    pub ty: ParamType,
    pub required: bool,
    pub desc: &'static str,
    /// 可选值的枚举。空表示不限。
    pub values: &'static [&'static str],
}

impl ParamSpec {
    /// 最简单的写法：必填字符串。
    pub const fn required(name: &'static str, desc: &'static str) -> Self {
        Self {
            name,
            ty: ParamType::String,
            required: true,
            desc,
            values: &[],
        }
    }

    /// 可选参数。
    pub const fn optional(name: &'static str, ty: ParamType, desc: &'static str) -> Self {
        Self {
            name,
            ty,
            required: false,
            desc,
            values: &[],
        }
    }

    /// 形状不固定的必填参数（OneBot 那种消息段）。
    pub const fn any(name: &'static str, desc: &'static str) -> Self {
        Self {
            name,
            ty: ParamType::Any,
            required: true,
            desc,
            values: &[],
        }
    }

    /// 形状不固定的可选参数。
    pub const fn maybe_any(name: &'static str, desc: &'static str) -> Self {
        Self {
            name,
            ty: ParamType::Any,
            required: false,
            desc,
            values: &[],
        }
    }

    /// 渲染成 JSON Schema 的一个属性。
    pub fn to_schema(&self) -> Value {
        let mut schema = serde_json::Map::new();
        // Any 不写 type：JSON Schema 里「什么形状都行」就是不约束。
        if self.ty != ParamType::Any {
            schema.insert("type".into(), Value::String(self.ty.json_type().into()));
        }
        schema.insert("description".into(), Value::String(self.desc.into()));
        if !self.values.is_empty() {
            schema.insert(
                "enum".into(),
                Value::Array(
                    self.values
                        .iter()
                        .map(|value| Value::String((*value).into()))
                        .collect(),
                ),
            );
        }
        Value::Object(schema)
    }

    /// 这个参数在样例里放什么。**填的是看得懂的假值**，不是 `<string>` 这种占位符——
    /// 占位符还得让人翻译一遍，填好的号改一改就能跑。
    pub fn sample(&self) -> Value {
        // 有枚举就取第一个：那是这个参数唯一正确的取值范围。
        if let Some(first) = self.values.first() {
            return Value::String((*first).into());
        }
        match self.ty {
            ParamType::Integer => serde_json::json!(10),
            ParamType::Bool => serde_json::json!(false),
            ParamType::StringList => serde_json::json!(["10000"]),
            ParamType::Object => serde_json::json!({}),
            // 形状不固定的那些：按名字猜最常见的样子。猜不准也没关系，
            // 它是样例不是约束——参数表里写着真实形状。
            ParamType::Any => match self.name {
                "message" => serde_json::json!([{ "type": "text", "text": "你好" }]),
                "messages" => serde_json::json!([{
                    "type": "node",
                    "data": {
                        "name": "某人",
                        "uin": "10000",
                        "content": [{ "type": "text", "text": "你好" }],
                    }
                }]),
                _ => serde_json::json!({}),
            },
            ParamType::String => Value::String(sample_string(self.name).to_owned()),
        }
    }
}

/// 字符串参数的样例值。按名字给最像真的那个：群号、QQ 号、文件地址。
fn sample_string(name: &str) -> &'static str {
    match name {
        "group_id" | "group" => "123456",
        "user_id" | "uin" | "qq" | "target" | "user" => "10000",
        "file" | "url" | "image" => "https://example.com/a.png",
        "text" | "message" => "你好",
        "name" | "card" | "nickname" => "新名字",
        "keyword" | "query" | "word" => "关键词",
        _ if name == "id" || name.ends_with("_id") => "123456",
        _ => "值",
    }
}

/// 参数类型。只留模型真用得上、适配器真能转的几种。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    String,
    Integer,
    Bool,
    StringList,
    Object,
    /// 形状不固定（OneBot 的消息段：可能是字符串，也可能是数组）。不校验，原样转交。
    ///
    /// 用它是**明确的取舍**：这类参数只做存在性检查，错了让平台那边报。
    /// 硬编成一种形状反而会把合法写法挡在外面。
    Any,
}

impl ParamType {
    pub fn json_type(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Bool => "boolean",
            Self::StringList => "array",
            Self::Object => "object",
            // JSON Schema 里「任意」就是不写 type，这里给个占位，调用方要跳过它。
            Self::Any => "",
        }
    }

    /// 这个 JSON 值算不算符合类型。只做到「大类型对得上」，不做深层校验。
    pub fn matches(self, value: &Value) -> bool {
        match self {
            Self::String => value.is_string(),
            Self::Integer => value.is_i64() || value.is_u64(),
            Self::Bool => value.is_boolean(),
            Self::StringList => {
                value.is_array()
                    && value
                        .as_array()
                        .is_some_and(|items| items.iter().all(Value::is_string))
            }
            Self::Object => value.is_object(),
            Self::Any => true,
        }
    }
}

/// 建议的用途标签。
///
/// 标签是**跨归属**的维度：`message.send` 和 `image.upload` 都挂 `image`，模型说「图片相关」
/// 就能一次捞齐，不必知道平台把这些散在哪个命名空间里。
///
/// 词表只列跨平台都成立的概念，因为这是 core 唯一该懂的那种知识。适配器可以自己加，
/// 但优先用这里的词——两个平台用同一个词，模型对同一个需求才只需要问一次。
pub mod tags {
    // 用途
    pub const SEND: &str = "send";
    pub const RECALL: &str = "recall";
    pub const FORWARD: &str = "forward";
    pub const HISTORY: &str = "history";
    pub const UPLOAD: &str = "upload";
    pub const DOWNLOAD: &str = "download";
    pub const APPROVE: &str = "approve";
    pub const POKE: &str = "poke";

    // 对象
    pub const MESSAGE: &str = "message";
    pub const MEMBER: &str = "member";
    pub const GROUP: &str = "group";
    pub const FRIEND: &str = "friend";
    pub const ACCOUNT: &str = "account";

    // 内容类型
    pub const MEDIA: &str = "media";
    pub const TEXT: &str = "text";
    pub const IMAGE: &str = "image";
    pub const VOICE: &str = "voice";
    pub const VIDEO: &str = "video";
    pub const FILE: &str = "file";

    // 管理
    pub const ADMIN: &str = "admin";
    pub const MUTE: &str = "mute";

    // 系统
    pub const SYSTEM: &str = "system";
}

/// 同一个概念的各种叫法，一行一组。
///
/// 模型是照需求说的（「图片」「语音」），适配器声明的是标识符（`image`、`voice`）。
/// 这张表把两头接上，省掉适配器为每个方法写一堆中文别名的负担。
const ALIASES: &[&[&str]] = &[
    &["image", "图片", "图", "照片", "截图", "pic", "picture"],
    &["voice", "语音", "音频", "录音", "audio"],
    &["video", "视频", "录像"],
    &["file", "文件", "附件", "群文件"],
    &["media", "媒体", "多模态"],
    &["message", "消息", "信息", "历史消息"],
    &["forward", "转发", "合并转发", "聊天记录"],
    &["send", "发送", "发消息", "说话"],
    &["recall", "撤回", "撤销"],
    &["history", "历史", "记录", "上下文"],
    &["member", "成员", "群员", "群成员", "成员列表"],
    &["group", "群", "群聊", "群组"],
    &["friend", "好友", "朋友", "私聊"],
    &["account", "账号", "资料", "个人信息"],
    &["admin", "管理", "管理员", "权限"],
    &["mute", "禁言", "闭嘴"],
    &["approve", "审批", "同意", "通过", "申请"],
    &["poke", "拍一拍", "戳一戳", "戳"],
    &["upload", "上传"],
    &["download", "下载"],
    &["system", "系统", "状态"],
];

/// 把一个查询词展开成一组同义检索词（中英都算），第一个永远是原词。
///
/// 判断用双向包含：「图片相关」能对上 `image`，「图」也能；代价是短词会多命中几行，
/// 但命中多了模型自己会挑，比漏掉强。
pub fn expand_query(needle: &str) -> Vec<String> {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut terms = vec![needle.clone()];
    for row in ALIASES {
        let hit = row.iter().any(|term| {
            let term = term.to_lowercase();
            needle.contains(&term) || term.contains(&needle)
        });
        if !hit {
            continue;
        }
        for term in row.iter() {
            let term = term.to_lowercase();
            if !terms.contains(&term) {
                terms.push(term);
            }
        }
    }
    terms
}

/// 一个目标参数的角色。
///
/// - `Room`：指向一个房间（`group_id` / `user_id`），必须和当前房间一致；
///   要换房间只能用 `platform_call` 的 `room` 参数。
/// - `Member`：指向房间里的一个成员/消息对象（「拍谁」「踢谁」），
///   不受房间目标校验影响。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetRole {
    Room,
    Member,
}

/// 声明一个参数和房间路径的关系。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetSpec {
    pub param: &'static str,
    /// 房间路径的第一段，如 `group` / `private`。由适配器声明，core 只比对。
    pub room_kind: &'static str,
    pub role: TargetRole,
}

impl TargetSpec {
    /// 这个参数是房间目标：在 `room_kind` 类型的房间里必须等于房间的 id。
    pub const fn room(param: &'static str, room_kind: &'static str) -> Self {
        Self {
            param,
            room_kind,
            role: TargetRole::Room,
        }
    }

    /// 这个参数是成员/消息目标：同一个参数在某些房间类型里可能是“拍谁/踢谁”，
    /// 不能按房间目标校验。
    pub const fn member(param: &'static str, room_kind: &'static str) -> Self {
        Self {
            param,
            room_kind,
            role: TargetRole::Member,
        }
    }
}

/// 一个平台方法的声明。适配器自己给，core 只认这个形状。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodSpec {
    /// 全名，强制 `resource.method`（`forward.get_content`）。
    pub name: &'static str,
    pub summary: &'static str,
    /// 用途标签，跨归属表达「这是干什么的」。从 [`tags`] 里挑。
    pub tags: &'static [&'static str],
    pub risk: Risk,
    /// 这个能力是「房间里的事」还是「账号上的事」。
    ///
    /// 好友列表、账号资料这类跟房间无关：模型不该为了查好友列表先编一个群号出来。
    pub scope: Scope,
    pub params: &'static [ParamSpec],
    /// 哪些参数是房间目标、哪些是成员目标。空 = 这个方法没有房间目标参数。
    pub targets: &'static [TargetSpec],
}

impl MethodSpec {
    /// 归属：名字里点号前面那段。跟平台 API 的命名空间对齐，模型看到 `{"type":"forward"}`
    /// 就能联想到 `forward.*`。
    pub fn group(&self) -> &'static str {
        self.name.split('.').next().unwrap_or(self.name)
    }

    /// 带不带这个标签。大小写不敏感——模型未必记得住声明时是大写还是小写。
    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|item| item.eq_ignore_ascii_case(tag))
    }

    /// 这个词命中不命中：名字、说明、标签任一含它都算。
    pub fn matches(&self, needle: &str) -> bool {
        let needle = needle.to_lowercase();
        self.name.to_lowercase().contains(&needle)
            || self.summary.to_lowercase().contains(&needle)
            || self.tags.iter().any(|tag| tag.to_lowercase().contains(&needle))
    }

    /// 参数表渲染成 JSON Schema。
    ///
    /// 内部是简化结构，给模型看的是它最熟的形式——模型照着填的准确率比读一张自定义表高。
    pub fn params_schema(&self) -> Value {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        for param in self.params {
            properties.insert(param.name.to_owned(), param.to_schema());
            if param.required {
                required.push(Value::String(param.name.to_owned()));
            }
        }
        let mut schema = serde_json::Map::new();
        schema.insert("type".into(), Value::String("object".into()));
        schema.insert("properties".into(), Value::Object(properties));
        if !required.is_empty() {
            schema.insert("required".into(), Value::Array(required));
        }
        Value::Object(schema)
    }

    /// 一次**能直接照抄**的调用样例。
    ///
    /// 只有参数表的话，模型得自己把 Schema 翻译成一次调用——翻译错就得试一轮。
    /// 这里照必填参数填好一张，改掉里面的号就能用（ADR-0024）。
    ///
    /// 只给必填参数：可选参数塞进去会让样例变长，反而不好抄。
    pub fn example_call(&self) -> Value {
        let mut params = serde_json::Map::new();
        for param in self.params.iter().filter(|param| param.required) {
            params.insert(param.name.to_owned(), param.sample());
        }
        serde_json::json!({
            "method": self.name,
            "params": Value::Object(params),
        })
    }
}

/// 一个方法的适用边界。决定调它的时候 `room` 是不是必填、按什么粒度校验。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// 房间级：在这个房间里做什么（读转发正文、撤回、查群成员）。`room` 缺省用当前轮次的房间，
    /// 给了就必须是会话订阅过的房间。
    Room,
    /// 连接级：这个号上的事，跟哪个房间无关（好友列表、账号资料、平台版本）。
    /// `room` 可以不提；提了只用来选连接，校验的是「这个连接有房间在我的订阅里」。
    Connection,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Room => "room",
            Self::Connection => "connection",
        }
    }
}

/// 方法的风险级别。破坏性的默认关，配置里按连接显式打开。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// 只读：查转发正文、查群成员。默认可用。
    Read,
    /// 写：发消息、撤回自己刚发的。默认可用。
    Write,
    /// 破坏性：踢人、禁言、撤回别人的。**默认关闭**。
    Destructive,
}

impl Risk {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Destructive => "destructive",
        }
    }
}

/// 调平台方法失败。五种情况分开报，agent 才能自己纠错（ADR-0020）。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CallError {
    /// 方法名不存在。`suggestion` 是最近似的名字，让模型一轮就能改对。
    #[error("没有这个方法：{method}")]
    UnknownMethod {
        method: String,
        suggestion: Option<String>,
    },
    /// 这个平台本身没这能力（Satori 里对应 404）。
    #[error("平台不支持：{0}")]
    UnsupportedPlatform(String),
    /// 平台有，我们的适配器还没写（Satori 里对应 501）。
    #[error("适配器还没实现：{0}")]
    NotImplemented(String),
    /// 有这能力，但这个号没权限，或者按配置没开给模型。
    #[error("不允许：{0}")]
    Forbidden(String),
    /// 参数不对。
    #[error("参数不对：{0}")]
    BadParams(String),
    /// 通道不通：连接断了、等平台回话超时。
    #[error("通道不可用：{0}")]
    Unavailable(String),
}

/// 出站通道。一条连接一个实现，由适配器提供。
pub trait Outbound: Send {
    /// 发一条到 `room` 指向的连接与位置。
    fn send(&mut self, room: &RoomId, elements: &[Element]) -> Result<Sent, OutboundError>;

    /// 这条连接声明了哪些平台方法。
    ///
    /// 默认空表——只有实现了平台特有能力才需要覆盖。`connection` 由分发层传进来，
    /// 单连接的适配器可以不理；[`Outbounds`] 用它挑对应的那条。
    fn catalog(&self, connection: &str) -> &'static [MethodSpec] {
        let _ = connection;
        &[]
    }

    /// 调一个平台方法，返回平台原始结果。
    ///
    /// 白名单、风险、参数形状的校验都由 kernel 先做完，走到这里的方法一定是声明过的、
    /// 参数形状也对得上的。适配器只管「把方法映射成平台动作、把结果原样带回来」。
    fn call(&mut self, room: &RoomId, method: &str, params: &Value) -> Result<Value, CallError> {
        let _ = (room, params);
        Err(CallError::UnsupportedPlatform(format!(
            "这条连接（{}）没有声明平台方法，调不了 {method}",
            room.connection
        )))
    }
}

/// 按连接分发的一组出站通道。
///
/// 一个设备可以同时订 QQ 群和 Discord 频道，所以桥不能只挂一个出站实现——发往哪个房间，
/// 就交给那个房间第一段指向的连接。没接连接的名字明确报错，不静默丢。
#[derive(Default)]
pub struct Outbounds {
    by_connection: std::collections::HashMap<String, Box<dyn Outbound>>,
}

impl Outbounds {
    pub fn new() -> Self {
        Self::default()
    }

    /// 给某条连接接上一个出站通道。同名重复接会覆盖前一个（配置层已经拦了重名）。
    pub fn with(mut self, connection: impl Into<String>, outbound: Box<dyn Outbound>) -> Self {
        self.by_connection.insert(connection.into(), outbound);
        self
    }

    /// 已经接了哪些连接。
    pub fn connections(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.by_connection.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }
}

impl Outbound for Outbounds {
    fn send(&mut self, room: &RoomId, elements: &[Element]) -> Result<Sent, OutboundError> {
        match self.by_connection.get_mut(&room.connection) {
            Some(outbound) => outbound.send(room, elements),
            None => Err(OutboundError::Unavailable(format!(
                "没有名为 {} 的连接，发不到 {}",
                room.connection, room
            ))),
        }
    }

    fn catalog(&self, connection: &str) -> &'static [MethodSpec] {
        match self.by_connection.get(connection) {
            Some(outbound) => outbound.catalog(connection),
            // 没接这条连接：空表。上层会报「没有这个连接」，不用在这编一套。
            None => &[],
        }
    }

    fn call(&mut self, room: &RoomId, method: &str, params: &Value) -> Result<Value, CallError> {
        match self.by_connection.get_mut(&room.connection) {
            Some(outbound) => outbound.call(room, method, params),
            None => Err(CallError::Unavailable(format!(
                "没有名为 {} 的连接，调不到 {}",
                room.connection, room
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 记下发过什么，不真发。
    #[derive(Default)]
    struct Recorder {
        sent: Vec<String>,
    }

    impl Outbound for Recorder {
        fn send(&mut self, room: &RoomId, _elements: &[Element]) -> Result<Sent, OutboundError> {
            self.sent.push(room.to_string());
            Ok(Sent {
                platform_id: "p1".into(),
                at: 0,
            })
        }
    }

    #[test]
    fn routes_by_connection() {
        let mut outbounds = Outbounds::new()
            .with("qq", Box::new(Recorder::default()))
            .with("qq2", Box::new(Recorder::default()));

        outbounds
            .send(
                &RoomId::new("qq", "group/1").expect("合法"),
                &[Element::text("x")],
            )
            .expect("能发");
        outbounds
            .send(
                &RoomId::new("qq2", "group/1").expect("合法"),
                &[Element::text("y")],
            )
            .expect("能发");

        // 第二个号在同一个群里：房间不同（连接不同），各发各的，不互相覆盖。
        assert_eq!(outbounds.connections(), vec!["qq", "qq2"]);
    }

    #[test]
    fn unconnected_room_is_an_error() {
        let mut outbounds = Outbounds::new();
        let room = RoomId::new("kook", "1").expect("合法");
        let error = outbounds
            .send(&room, &[Element::text("x")])
            .expect_err("没接连接就该报错");
        assert!(matches!(error, OutboundError::Unavailable(_)));
    }

    /// 默认不声明任何方法、也调不动：没平台能力的适配器（比如纯测试 mock）不用写任何新代码。
    #[test]
    fn default_catalog_is_empty() {
        let mut outbounds = Outbounds::new().with("qq", Box::new(Recorder::default()));
        assert!(outbounds.catalog("qq").is_empty());

        let room = RoomId::new("qq", "group/1").expect("合法");
        let error = outbounds
            .call(&room, "forward.get_content", &serde_json::json!({"id": "x"}))
            .expect_err("没声明就调不动");
        assert!(matches!(error, CallError::UnsupportedPlatform(_)));
    }

    /// 声明了方法的适配器：目录按连接挑，调用落到对的那条。
    #[derive(Default)]
    struct Platformy;

    static FORWARD_PARAMS: &[ParamSpec] = &[ParamSpec {
        name: "id",
        ty: ParamType::String,
        required: true,
        desc: "转发 id",
        values: &[],
    }];

    static METHODS: &[MethodSpec] = &[MethodSpec {
        name: "forward.get_content",
        summary: "读合并转发的正文",
        tags: &[tags::FORWARD, tags::MESSAGE],
        risk: Risk::Read,
        scope: Scope::Room,
        params: FORWARD_PARAMS,
        targets: &[],
    }];

    impl Outbound for Platformy {
        fn send(&mut self, _room: &RoomId, _elements: &[Element]) -> Result<Sent, OutboundError> {
            Ok(Sent {
                platform_id: "p".into(),
                at: 0,
            })
        }

        fn catalog(&self, _connection: &str) -> &'static [MethodSpec] {
            METHODS
        }

        fn call(
            &mut self,
            _room: &RoomId,
            method: &str,
            params: &Value,
        ) -> Result<Value, CallError> {
            Ok(serde_json::json!({ "called": method, "params": params }))
        }
    }

    #[test]
    fn outbounds_dispatch_catalog_by_connection() {
        let mut outbounds = Outbounds::new()
            .with("qq", Box::new(Platformy))
            .with("qq2", Box::new(Recorder::default()));

        // 只有声明过的那条连接看得见方法。
        assert_eq!(outbounds.catalog("qq").len(), 1);
        assert_eq!(outbounds.catalog("qq").first().map(MethodSpec::group), Some("forward"));
        assert!(outbounds.catalog("qq2").is_empty());
        assert!(outbounds.catalog("没这个连接").is_empty());

        let room = RoomId::new("qq", "group/1").expect("合法");
        let result = outbounds
            .call(&room, "forward.get_content", &serde_json::json!({"id": "x7"}))
            .expect("声明过就能调");
        assert_eq!(result["called"], "forward.get_content");
        assert_eq!(result["params"]["id"], "x7");

        // 另一条连接没声明，落到默认实现：报平台不支持，而不是静默成功。
        let other = RoomId::new("qq2", "group/1").expect("合法");
        let error = outbounds
            .call(&other, "forward.get_content", &serde_json::json!({}))
            .expect_err("没声明就调不动");
        assert!(matches!(error, CallError::UnsupportedPlatform(_)));
    }

    /// 名字里点号前面那段就是归属，目录靠它在清单里排序认路。
    #[test]
    fn method_group_is_prefix() {
        let spec = MethodSpec {
            name: "group.get_member_info",
            summary: "",
            tags: &[],
            risk: Risk::Read,
            scope: Scope::Room,
            params: &[],
            targets: &[],
        };
        assert_eq!(spec.group(), "group");
    }

    /// 标签跨归属：两个不同命名空间的方法挂同一个 tag，是这个维度存在的全部理由。
    #[test]
    fn tag_cuts_across_groups() {
        static MIXED: &[MethodSpec] = &[
            MethodSpec {
                name: "message.send",
                summary: "发一条消息",
                tags: &[tags::SEND, tags::IMAGE],
                risk: Risk::Write,
                scope: Scope::Room,
                params: &[],
                targets: &[],
            },
            MethodSpec {
                name: "group.get_member_info",
                summary: "查群成员",
                tags: &[tags::MEMBER],
                risk: Risk::Read,
                scope: Scope::Room,
                params: &[],
                targets: &[],
            },
        ];

        let pictures: Vec<&str> = MIXED
            .iter()
            .filter(|method| method.has_tag("image"))
            .map(|method| method.name)
            .collect();
        assert_eq!(pictures, vec!["message.send"]);
        assert_eq!(MIXED[0].group(), "message");
    }

    /// 模型说中文，适配器声明英文标识符：查询词得两头都能接上。
    #[test]
    fn query_terms_bridge_chinese_and_identifiers() {
        let terms = expand_query("图片");
        assert!(terms.contains(&"image".to_owned()), "中文要能对到标识符");
        assert!(terms.contains(&"图片".to_owned()), "原词一定要留着");

        // 反过来：模型照文档说英文，也得能搜到中文说明。
        let terms = expand_query("voice");
        assert!(terms.contains(&"语音".to_owned()));

        // 带修饰的说法也要命中。
        assert!(expand_query("图片相关").contains(&"image".to_owned()));

        // 表里没有的词不能凭空消失，原样带回去让子串匹配兜底。
        assert_eq!(expand_query("qwq"), vec!["qwq".to_owned()]);
        assert!(expand_query("   ").is_empty());
    }

    /// 参数类型只做「大类型对得上」，够挡住把字符串塞进 integer 这类明显错。
    #[test]
    fn param_type_matches_basic_shapes() {
        assert!(ParamType::String.matches(&serde_json::json!("x")));
        assert!(!ParamType::String.matches(&serde_json::json!(1)));
        assert!(ParamType::Integer.matches(&serde_json::json!(7)));
        assert!(!ParamType::Integer.matches(&serde_json::json!(7.5)));
        assert!(ParamType::Bool.matches(&serde_json::json!(true)));
        assert!(ParamType::StringList.matches(&serde_json::json!(["a", "b"])));
        assert!(!ParamType::StringList.matches(&serde_json::json!(["a", 1])));
        assert!(ParamType::Object.matches(&serde_json::json!({"a": 1})));
    }
}
