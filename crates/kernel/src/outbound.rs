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

    /// 渲染成 JSON Schema 的一个属性。
    pub fn to_schema(&self) -> Value {
        let mut schema = serde_json::Map::new();
        schema.insert("type".into(), Value::String(self.ty.json_type().into()));
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
}

/// 参数类型。只留模型真用得上、适配器真能转的几种。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    String,
    Integer,
    Bool,
    StringList,
    Object,
}

impl ParamType {
    pub fn json_type(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Bool => "boolean",
            Self::StringList => "array",
            Self::Object => "object",
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
        }
    }
}

/// 一个平台方法的声明。适配器自己给，core 只认这个形状。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodSpec {
    /// 全名，强制 `resource.method`（`forward.get_content`）。
    pub name: &'static str,
    pub summary: &'static str,
    pub risk: Risk,
    pub params: &'static [ParamSpec],
}

impl MethodSpec {
    /// 分组名：名字里点号前面那段。目录按它两层披露。
    pub fn group(&self) -> &'static str {
        self.name.split('.').next().unwrap_or(self.name)
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
        risk: Risk::Read,
        params: FORWARD_PARAMS,
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

    /// 名字里点号前面那段就是分组名，目录靠它两层披露。
    #[test]
    fn method_group_is_prefix() {
        let spec = MethodSpec {
            name: "group.get_member_info",
            summary: "",
            risk: Risk::Read,
            params: &[],
        };
        assert_eq!(spec.group(), "group");
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
