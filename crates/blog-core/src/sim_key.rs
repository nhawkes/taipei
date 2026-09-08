//! The sim fence's wire vocabulary — the one contract the content mapping (which
//! writes live markers) and the sim's live half (which parses its key) share. Kebab
//! tokens are both the fence vocabulary (`serde(rename_all)`) and the live-key wire
//! tokens (`strum`), so an unknown name is a parse error, not a check.

/// A leaf-server behavior a sim can offer as a tab.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Deserialize, strum::EnumString, strum::IntoStaticStr)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum ServerBehavior {
    Good,
    NeverAccept,
    AcceptHang,
}

impl ServerBehavior {
    /// The stable identifier — the fence name and the live-key wire token.
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// The human-readable tab label.
    pub fn label(self) -> &'static str {
        match self {
            ServerBehavior::Good => "good server",
            ServerBehavior::NeverAccept => "never accept",
            ServerBehavior::AcceptHang => "accept then hang",
        }
    }
}

/// A protection composition a sim can run — the taipei `Stage` vocabulary, named here
/// so a fence is parsed into it and the app converts a value rather than a string.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Deserialize, strum::EnumString, strum::IntoStaticStr)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum PolicyStage {
    /// The bare service, unprotected.
    App,
    /// CPU backpressure only.
    Backpressure,
    /// A concurrency limit that sheds immediately when it is full.
    Reject,
    /// The same limit with nothing above it: callers wait for a slot, unboundedly.
    Wait,
    /// The full stack — a queue that sheds on its deadline.
    Queue,
}

impl PolicyStage {
    /// The stable identifier — the fence name and the live-key wire token.
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// One policy tab: what the reader is shown, and which composition that runs. Written
/// out rather than paired by position, so a fence cannot silently mis-align the two.
#[derive(Clone, PartialEq, Eq, Hash, Debug, serde::Deserialize)]
pub struct PolicyTab {
    /// The label on the pill, and the code-group tab this policy's listing comes from.
    pub display: String,
    /// The composition the label names.
    pub stage: PolicyStage,
}

/// An admission-gate signal a sim can offer as a tab — the "when can the server take
/// more work" policies the gate chapter compares. Kebab names are the fence vocabulary
/// and the live-key wire tokens, like [`ServerBehavior`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Deserialize, strum::EnumString, strum::IntoStaticStr)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum GateSignal {
    /// A fixed in-flight ceiling, held for the whole request.
    ConcurrencyLimit,
    /// OS-level CPU utilisation over a trailing window — cheap, uninvasive, delayed.
    OsCpu,
    /// Runtime-level CPU tracking: admit while under half the cores are active.
    RuntimeCpu,
}

impl GateSignal {
    /// The stable identifier — the fence name and the live-key wire token.
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// The human-readable tab label.
    pub fn label(self) -> &'static str {
        match self {
            GateSignal::ConcurrencyLimit => "concurrency limit",
            GateSignal::OsCpu => "OS CPU %",
            GateSignal::RuntimeCpu => "runtime CPU",
        }
    }
}

/// The control a sim's prose invites the reader to move (`"try": "arrivals"` in the
/// fence) — the view highlights it. Kebab names are the fence vocabulary and the wire
/// tokens; an unknown one is a parse error.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Deserialize, strum::EnumString, strum::IntoStaticStr)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum TryControl {
    Arrivals,
    IoSpeed,
}

/// A ` ```sim ` fence's live key: the fence ordinal (mount identity — two identical
/// fences stay distinct mounts) plus the authored configuration parsed from the fence's
/// JSON body. The key IS the sim's data entry — edit the fence and the live remounts
/// with the new spec; nothing rides the page node.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SimKey {
    pub ordinal: u32,
    pub width: u32,
    pub height: u32,
    /// The composition the fence named. `None` is a fence that named none — which is
    /// every sim but the visualiser, since a fleet or a fan-out runs no single stage.
    ///
    /// A value, not a token: an unspellable stage is a content error the mapping raises,
    /// so a reader of this field chooses between compositions that exist, and never has
    /// to decide what an unrecognised name meant.
    pub stage: Option<PolicyStage>,
    /// Empty string = the sim's default workload.
    pub workload: String,
    /// Draw the sparkline charts (default `true`).
    pub charts: bool,
    /// User-driven load: no automatic arrivals, requests come from the "send a
    /// request" button (default `false`).
    pub manual: bool,
    /// Selectable leaf-server behaviors, shown as tabs (empty = a single good server).
    pub servers: Vec<ServerBehavior>,
    /// Selectable admission-gate signals, shown as tabs (empty = the stage's own gate).
    pub gates: Vec<GateSignal>,
    /// The control the prose invites the reader to move — highlighted in the view.
    pub try_control: Option<TryControl>,
    /// Policy tabs: the compositions the reader can switch the sim between, each with
    /// the label it wears. Empty = just the one stage the fence named.
    pub tabs: Vec<PolicyTab>,
    /// The code group whose tabs label those policies, by id — empty when no tab group
    /// sits above the fence. The code is page data; this is the reference to it.
    pub code: String,
}

impl SimKey {
    /// The key's wire spelling — the inverse of
    /// [`FromLiveKey::from_wire`](idyll::live::FromLiveKey::from_wire).
    pub fn wire(&self) -> String {
        let SimKey {
            ordinal, width, height, stage, workload, charts, manual, servers, gates, try_control,
            tabs, code,
        } = self;
        // Behavior/gate/stage tokens are a fixed identifier set (`[a-z-]`), so joining
        // needs no escaping; the outer key stays `;`-delimited. A tab's label is
        // authored, so the fence parser rejects the three characters that would break
        // this — the `display:stage` pair separator included.
        let servers = servers.iter().map(|b| b.as_str()).collect::<Vec<_>>().join(",");
        let gates = gates.iter().map(|g| g.as_str()).collect::<Vec<_>>().join(",");
        let try_control = try_control.map(<&str>::from).unwrap_or_default();
        let stage = stage.map(<&str>::from).unwrap_or_default();
        let tabs = tabs
            .iter()
            .map(|t| format!("{}:{}", t.display, t.stage.as_str()))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{ordinal};{width};{height};{stage};{workload};{};{};{servers};{gates};{try_control};{tabs};{code}",
            *charts as u8, *manual as u8
        )
    }
}

/// A sim key is also writable *from* a view — the policy pill mounts the stage it
/// picked by handing it a key, so switching policy is a change of identity (a fresh
/// machine at the new stage) rather than a machine talked into being a different one.
impl idyll::live::IslandKey for SimKey {
    fn to_wire(&self) -> String {
        self.wire()
    }
}

impl idyll::live::FromLiveKey for SimKey {
    fn from_wire(wire: &str) -> Self {
        let mut fields = wire.splitn(12, ';');
        let mut next = || fields.next().unwrap_or_else(|| panic!("sim key `{wire}` is not `ordinal;width;height;stage;workload;charts;manual;servers;gates;try;tabs;code`"));
        let num = |field: &str| field.parse().unwrap_or_else(|_| panic!("sim key `{wire}`: `{field}` is not a number"));
        SimKey {
            ordinal: num(next()),
            width: num(next()),
            height: num(next()),
            stage: next().parse().ok(),
            workload: next().to_string(),
            charts: next() == "1",
            manual: next() == "1",
            servers: next().split(',').filter_map(|t| t.parse().ok()).collect(),
            gates: next().split(',').filter_map(|t| t.parse().ok()).collect(),
            try_control: next().parse().ok(),
            tabs: next()
                .split(',')
                .filter_map(|t| t.split_once(':'))
                .filter_map(|(display, stage)| {
                    Some(PolicyTab { display: display.to_string(), stage: stage.parse().ok()? })
                })
                .collect(),
            code: next().to_string(),
        }
    }
}
