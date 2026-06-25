use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use crossbeam_channel::Receiver;
use muda::MenuEvent;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedValue, Structure, Type, Value};

use crate::menu::MenuId;
use crate::TrayIconId;

pub const SNI_PATH: ObjectPath = ObjectPath::from_static_str_unchecked("/StatusNotifierItem");
pub const MENU_PATH: ObjectPath = ObjectPath::from_static_str_unchecked("/MenuBar");

fn rgba_to_argb32(rgba: &[u8]) -> Vec<u8> {
    let mut data = rgba.to_vec();
    for pixel in data.chunks_exact_mut(4) {
        // [R,G,B,A] → [A,R,G,B]  (ARGB32 in network byte order)
        pixel.rotate_right(1);
    }
    data
}

impl From<IconPixmap> for Value<'_> {
    fn from(icon: IconPixmap) -> Self {
        Value::from((icon.width, icon.height, icon.data))
    }
}

impl From<ToolTipData> for Structure<'_> {
    fn from(tooltip: ToolTipData) -> Self {
        (
            tooltip.icon_name,
            tooltip.icon_pixmap,
            tooltip.title,
            tooltip.description,
        )
            .into()
    }
}

#[derive(Debug, Clone)]
pub struct IconData {
    pub width: i32,
    pub height: i32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct MenuItemNode {
    pub id: MenuId,
    pub label: String,
    pub enabled: bool,
    pub checked: Option<bool>,
    pub is_separator: bool,
    pub is_submenu: bool,
    pub children: Vec<MenuItemNode>,
}

#[derive(Debug, Clone)]
pub struct MenuSnapshot {
    pub items: Vec<MenuItemNode>,
}

#[derive(Debug)]
pub enum Command {
    Update {
        id: TrayIconId,
        icon: Option<IconData>,
        tooltip: Option<String>,
        title: Option<String>,
        visible: bool,
        menu: Option<MenuSnapshot>,
    },
    Shutdown,
}

#[derive(Debug, Clone, Type, serde::Serialize)]
pub struct IconPixmap {
    pub width: i32,
    pub height: i32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Default, Type, serde::Serialize)]
pub struct ToolTipData {
    pub icon_name: String,
    pub icon_pixmap: Vec<IconPixmap>,
    pub title: String,
    pub description: String,
}

pub struct Inner {
    pub id: String,
    pub icon_pixmap: Vec<IconPixmap>,
    pub tooltip: Option<String>,
    pub title: Option<String>,
    pub status: String,
    pub menu: Option<MenuSnapshot>,
    pub revision: u32,
    pub item_id_offset: i32,
}

impl Inner {
    fn get_icon_pixmap(&self) -> Vec<IconPixmap> {
        self.icon_pixmap.clone()
    }

    fn get_tooltip(&self) -> ToolTipData {
        let text = self.tooltip.as_deref().unwrap_or("");
        ToolTipData {
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
            title: text.to_string(),
            description: String::new(),
        }
    }

    fn get_title(&self) -> String {
        self.title.as_deref().unwrap_or("").to_string()
    }

    fn flatten_items(&self) -> Vec<FlatItem> {
        let items = match &self.menu {
            Some(m) => &m.items,
            None => return vec![],
        };

        let mut result: Vec<FlatItem> =
            vec![FlatItem::root(items.len())];

        let mut stack = vec![(items.iter(), 0)];

        while let Some((mut iter, parent_idx)) = stack.pop() {
            while let Some(node) = iter.next() {
                let idx = result.len();
                let item = if node.is_separator {
                    FlatItem::separator(idx)
                } else if node.checked.is_some() {
                    FlatItem::check(&node.label, node.enabled, node.checked.unwrap_or(false))
                } else if node.is_submenu || !node.children.is_empty() {
                    FlatItem::submenu(&node.label, node.enabled, node.children.len())
                } else {
                    FlatItem::standard(&node.label, node.enabled)
                };
                result.push(item);
                result[parent_idx].children.push(idx);
                if !node.children.is_empty() {
                    stack.push((iter, parent_idx));
                    stack.push((node.children.iter(), idx));
                    break;
                }
            }
        }

        result
    }

    fn build_layout(
        &self,
        parent_id: i32,
        recursion_depth: Option<usize>,
        property_filter: &[String],
    ) -> Option<Layout> {
        let flat = self.flatten_items();
        if parent_id != 0 {
            return None;
        }

        let mut stack: Vec<(usize, usize, bool)> =
            vec![(0, 0, false)];
        let mut pending_children: Vec<Value<'static>> = Vec::new();

        while let Some((idx, depth, processed)) = stack.pop() {
            let item = &flat[idx];
            let reach_limit = recursion_depth.is_some_and(|limit| depth >= limit);

            if processed {
                let child_count = if reach_limit { 0 } else { item.children.len() };
                let children =
                    pending_children.split_off(pending_children.len() - child_count);
                let layout = Layout {
                    id: idx as i32,
                    properties: item.to_dbus_map(property_filter),
                    children,
                };
                if idx == 0 {
                    return Some(layout);
                }
                pending_children.push(layout.into());
            } else {
                stack.push((idx, depth, true));
                if !reach_limit {
                    for &child_idx in item.children.iter().rev() {
                        stack.push((child_idx, depth + 1, false));
                    }
                }
            }
        }
        unreachable!()
    }
}

#[derive(Clone)]
struct FlatItem {
    id: i32,
    label: String,
    enabled: bool,
    item_type: &'static str,
    toggle_state: Option<i32>,
    children: Vec<usize>,
}

impl FlatItem {
    fn root(child_count: usize) -> Self {
        Self {
            id: 0,
            label: String::new(),
            enabled: true,
            item_type: "standard",
            toggle_state: None,
            children: Vec::with_capacity(child_count),
        }
    }

    fn standard(label: &str, enabled: bool) -> Self {
        Self {
            id: 0,
            label: label.to_string(),
            enabled,
            item_type: "standard",
            toggle_state: None,
            children: Vec::new(),
        }
    }

    fn separator(_id: usize) -> Self {
        Self {
            id: 0,
            label: String::new(),
            enabled: true,
            item_type: "separator",
            toggle_state: None,
            children: Vec::new(),
        }
    }

    fn check(label: &str, enabled: bool, checked: bool) -> Self {
        Self {
            id: 0,
            label: label.to_string(),
            enabled,
            item_type: "standard",
            toggle_state: Some(if checked { 1 } else { 0 }),
            children: Vec::new(),
        }
    }

    fn submenu(label: &str, enabled: bool, _child_count: usize) -> Self {
        Self {
            id: 0,
            label: label.to_string(),
            enabled,
            item_type: "standard",
            toggle_state: None,
            children: Vec::new(),
        }
    }

    fn to_dbus_map(&self, property_filter: &[String]) -> HashMap<Cow<'static, str>, OwnedValue> {
        let filter = |name: &str| {
            property_filter.is_empty() || property_filter.iter().any(|s| s == name)
        };
        let mut map: HashMap<Cow<'static, str>, OwnedValue> = HashMap::new();

        if self.item_type == "separator" && filter("type") {
            map.insert(
                "type".into(),
                Value::Str("separator".into()).try_into().unwrap(),
            );
        }

        if !self.children.is_empty() && filter("children-display") {
            map.insert(
                "children-display".into(),
                Value::Str("submenu".into()).try_into().unwrap(),
            );
        }

        if !self.label.is_empty() && filter("label") {
            map.insert(
                "label".into(),
                Value::Str(self.label.clone().into()).try_into().unwrap(),
            );
        }

        if !self.enabled && filter("enabled") {
            map.insert("enabled".into(), OwnedValue::from(false));
        }

        if let Some(state) = self.toggle_state {
            if filter("toggle-type") {
                map.insert(
                    "toggle-type".into(),
                    Value::Str("checkmark".into()).try_into().unwrap(),
                );
            }
            if filter("toggle-state") {
                map.insert("toggle-state".into(), OwnedValue::from(state));
            }
        }

        map
    }
}

#[derive(Debug, Default, Type, serde::Serialize)]
pub(crate) struct Layout {
    pub id: i32,
    pub properties: HashMap<Cow<'static, str>, OwnedValue>,
    pub children: Vec<Value<'static>>,
}

impl TryFrom<Value<'static>> for Layout {
    type Error = zbus::zvariant::Error;
    fn try_from(value: Value<'static>) -> Result<Self, Self::Error> {
        let mut fields = zbus::zvariant::Structure::try_from(value)?.into_fields();
        Ok(Self {
            id: fields.remove(0).downcast()?,
            properties: fields.remove(0).downcast::<ItemPropsHelper>()?.0,
            children: fields.remove(0).downcast()?,
        })
    }
}

struct ItemPropsHelper(HashMap<Cow<'static, str>, OwnedValue>);

impl TryFrom<Value<'_>> for ItemPropsHelper {
    type Error = zbus::zvariant::Error;
    fn try_from(value: Value<'_>) -> Result<Self, Self::Error> {
        if let Value::Dict(dict) = value {
            let map = dict
                .into_iter()
                .map(|(k, v)| {
                    let key = String::try_from(k)
                        .map(Cow::Owned)
                        .unwrap_or(Cow::Borrowed(""));
                    let val = OwnedValue::try_from(v).unwrap_or(OwnedValue::from(false));
                    Ok::<_, zbus::zvariant::Error>((key, val))
                })
                .collect::<Result<HashMap<_, _>, _>>()?;
            Ok(ItemPropsHelper(map))
        } else {
            Err(zbus::zvariant::Error::IncorrectType)
        }
    }
}

impl<'a> From<Layout> for Value<'a> {
    fn from(l: Layout) -> Self {
        Value::from(
            zbus::zvariant::StructureBuilder::new()
                .add_field(l.id)
                .add_field(l.properties)
                .add_field(l.children)
                .build()
                .unwrap(),
        )
    }
}

impl From<Layout> for OwnedValue {
    fn from(l: Layout) -> Self {
        Value::from(l)
            .try_into_owned()
            .expect("Layout should not contain any fd")
    }
}

pub struct StatusNotifierItem(Arc<std::sync::Mutex<Inner>>);

impl StatusNotifierItem {
    pub fn new(inner: Arc<std::sync::Mutex<Inner>>) -> Self {
        Self(inner)
    }
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl StatusNotifierItem {
    async fn context_menu(&self, _x: i32, _y: i32) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::UnknownMethod(
            "Not supported, please use `menu`".into(),
        ))
    }

    async fn activate(&self, _x: i32, _y: i32) -> zbus::fdo::Result<()> {
        Ok(())
    }

    async fn secondary_activate(&self, _x: i32, _y: i32) -> zbus::fdo::Result<()> {
        Ok(())
    }

    async fn scroll(&self, _delta: i32, _dir: String) -> zbus::fdo::Result<()> {
        Ok(())
    }

    #[zbus(property)]
    async fn category(&self) -> zbus::fdo::Result<String> {
        Ok("ApplicationStatus".into())
    }

    #[zbus(property)]
    async fn id(&self) -> zbus::fdo::Result<String> {
        let inner = self.0.lock().unwrap();
        Ok(inner.id.clone())
    }

    #[zbus(property)]
    async fn title(&self) -> zbus::fdo::Result<String> {
        let inner = self.0.lock().unwrap();
        Ok(inner.get_title())
    }

    #[zbus(property)]
    async fn status(&self) -> zbus::fdo::Result<String> {
        let inner = self.0.lock().unwrap();
        Ok(inner.status.clone())
    }

    #[zbus(property)]
    async fn window_id(&self) -> zbus::fdo::Result<i32> {
        Ok(0)
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> zbus::fdo::Result<String> {
        Ok(String::new())
    }

    #[zbus(property)]
    fn menu(&self) -> zbus::fdo::Result<ObjectPath<'_>> {
        Ok(MENU_PATH)
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> zbus::fdo::Result<bool> {
        Ok(false)
    }

    #[zbus(property)]
    async fn icon_name(&self) -> zbus::fdo::Result<String> {
        Ok(String::new())
    }

    #[zbus(property)]
    async fn icon_pixmap(&self) -> zbus::fdo::Result<Vec<IconPixmap>> {
        let inner = self.0.lock().unwrap();
        Ok(inner.get_icon_pixmap())
    }

    #[zbus(property)]
    async fn overlay_icon_name(&self) -> zbus::fdo::Result<String> {
        Ok(String::new())
    }

    #[zbus(property)]
    async fn overlay_icon_pixmap(&self) -> zbus::fdo::Result<Vec<IconPixmap>> {
        Ok(Vec::new())
    }

    #[zbus(property)]
    async fn attention_icon_name(&self) -> zbus::fdo::Result<String> {
        Ok(String::new())
    }

    #[zbus(property)]
    async fn attention_icon_pixmap(&self) -> zbus::fdo::Result<Vec<IconPixmap>> {
        Ok(Vec::new())
    }

    #[zbus(property)]
    async fn attention_movie_name(&self) -> zbus::fdo::Result<String> {
        Ok(String::new())
    }

    #[zbus(property)]
    async fn tool_tip(&self) -> zbus::fdo::Result<ToolTipData> {
        let inner = self.0.lock().unwrap();
        Ok(inner.get_tooltip())
    }

    #[zbus(signal)]
    pub async fn new_title(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn new_icon(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn new_attention_icon(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn new_overlay_icon(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn new_tool_tip(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn new_status(ctxt: &SignalEmitter<'_>, status: &str) -> zbus::Result<()>;
}

pub struct DbusMenu(Arc<std::sync::Mutex<Inner>>);

impl DbusMenu {
    pub fn new(inner: Arc<std::sync::Mutex<Inner>>) -> Self {
        Self(inner)
    }
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl DbusMenu {
    async fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, Layout)> {
        let inner = self.0.lock().unwrap();
        let revision = inner.revision;
        let layout = inner.build_layout(
            parent_id,
            if recursion_depth < 0 {
                None
            } else {
                Some(recursion_depth as usize)
            },
            &property_names,
        );
        layout
            .map(|l| (revision, l))
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("parentId not found".to_string()))
    }

    async fn get_group_properties(
        &self,
        _ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> zbus::fdo::Result<Vec<(i32, HashMap<Cow<'static, str>, OwnedValue>)>> {
        Ok(Vec::new())
    }

    async fn get_property(&self, _id: i32, _name: String) -> zbus::fdo::Result<OwnedValue> {
        Err(zbus::fdo::Error::InvalidArgs("not implemented".into()))
    }

    async fn event(
        &self,
        id: i32,
        event_id: String,
        _data: OwnedValue,
        _timestamp: u32,
    ) -> zbus::fdo::Result<()> {
        if event_id == "clicked" && id != 0 {
            let inner = self.0.lock().unwrap();
            if let Some(menu) = &inner.menu {
                let flat_ids = flatten_ids(&menu.items);
                if let Some(item_id) = flat_ids.get(id as usize) {
                    if !item_id.is_empty() {
                        MenuEvent::send(MenuEvent {
                            id: MenuId::new(item_id),
                        });
                        return Ok(());
                    }
                }
            }
            Err(zbus::fdo::Error::InvalidArgs("id not found".into()))
        } else {
            Ok(())
        }
    }

    async fn event_group(
        &self,
        events: Vec<(i32, String, OwnedValue, u32)>,
    ) -> zbus::fdo::Result<Vec<i32>> {
        let mut not_found = Vec::new();
        for (id, event_id, data, timestamp) in events {
            if self.event(id, event_id, data, timestamp).await.is_err() {
                not_found.push(id);
            }
        }
        Ok(not_found)
    }

    async fn about_to_show(&self, id: i32) -> zbus::fdo::Result<bool> {
        let inner = self.0.lock().unwrap();
        if id == 0 && inner.menu.is_some() {
            Ok(false)
        } else if id != 0 {
            let flat = match &inner.menu {
                Some(m) => flatten_ids(&m.items),
                None => return Err(zbus::fdo::Error::InvalidArgs("id not found".into())),
            };
            if flat.get(id as usize).is_some() {
                Ok(false)
            } else {
                Err(zbus::fdo::Error::InvalidArgs("id not found".into()))
            }
        } else {
            Err(zbus::fdo::Error::InvalidArgs("id not found".into()))
        }
    }

    async fn about_to_show_group(
        &self,
        _ids: Vec<i32>,
    ) -> zbus::fdo::Result<(Vec<i32>, Vec<i32>)> {
        Ok((Vec::new(), Vec::new()))
    }

    #[zbus(property)]
    fn version(&self) -> zbus::fdo::Result<u32> {
        Ok(3)
    }

    #[zbus(property)]
    fn text_direction(&self) -> zbus::fdo::Result<String> {
        Ok("ltr".into())
    }

    #[zbus(property)]
    fn status(&self) -> zbus::fdo::Result<String> {
        Ok("normal".into())
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> zbus::fdo::Result<Vec<String>> {
        Ok(Vec::new())
    }

    #[zbus(signal)]
    pub async fn items_properties_updated(
        ctxt: &SignalEmitter<'_>,
        updated_props: Vec<(i32, HashMap<Cow<'static, str>, OwnedValue>)>,
        removed_props: Vec<(i32, Vec<Cow<'static, str>>)>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn layout_updated(
        ctxt: &SignalEmitter<'_>,
        revision: u32,
        parent: i32,
    ) -> zbus::Result<()>;
}

fn flatten_ids(items: &[MenuItemNode]) -> Vec<String> {
    let mut result = vec![String::new()];
    flatten_ids_recurse(items, &mut result);
    result
}

fn flatten_ids_recurse(items: &[MenuItemNode], result: &mut Vec<String>) {
    for item in items {
        result.push(item.id.0.clone());
        if !item.children.is_empty() {
            flatten_ids_recurse(&item.children, result);
        }
    }
}

#[zbus::proxy(
    interface = "org.kde.StatusNotifierWatcher",
    default_service = "org.kde.StatusNotifierWatcher",
    default_path = "/StatusNotifierWatcher"
)]
trait StatusNotifierWatcher {
    async fn register_status_notifier_item(&self, service: &str) -> zbus::Result<()>;
}

pub async fn run_service(
    rx: Receiver<Command>,
    tray_id: TrayIconId,
    initial_icon: Option<IconData>,
    initial_tooltip: Option<String>,
    initial_title: Option<String>,
) {
        let init_pixmaps: Vec<IconPixmap> = initial_icon
        .map(|d| {
            let data = rgba_to_argb32(&d.data);
            vec![IconPixmap {
                width: d.width,
                height: d.height,
                data,
            }]
        })
        .unwrap_or_default();
    let inner = Arc::new(std::sync::Mutex::new(Inner {
        id: format!("tray-icon-{}", tray_id.as_ref()),
        icon_pixmap: init_pixmaps,
        tooltip: initial_tooltip,
        title: initial_title,
        status: "Active".to_string(),
        menu: None,
        revision: 0,
        item_id_offset: 0,
    }));

    let sni = StatusNotifierItem::new(inner.clone());
    let menu_intf = DbusMenu::new(inner.clone());

    let conn = match zbus::connection::Builder::session()
        .expect("failed to create session connection builder")
        .serve_at(SNI_PATH, sni)
        .expect("SNI_PATH invalid")
        .serve_at(MENU_PATH, menu_intf)
        .expect("MENU_PATH invalid")
        .build()
        .await
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tray-icon: failed to create D-Bus connection: {e}");
            return;
        }
    };

    let name = format!(
        "org.kde.StatusNotifierItem-{}-{}",
        std::process::id(),
        tray_id.as_ref()
    );

    if let Err(e) = conn.request_name(name.as_str()).await {
        eprintln!("tray-icon: failed to request bus name {name}: {e}");
        // Continue anyway — watcher may use our unique name
    }

    let watcher = StatusNotifierWatcherProxy::new(&conn).await
        .expect("StatusNotifierWatcherProxy should be valid");

    if let Err(e) = watcher.register_status_notifier_item(&name).await {
        eprintln!("tray-icon: failed to register with watcher: {e}");
    }

    loop {
        match rx.recv() {
            Ok(Command::Shutdown) | Err(_) => {
                let _ = conn.close().await;
                break;
            }
            Ok(Command::Update {
                icon,
                tooltip,
                title,
                visible,
                menu,
                ..
            }) => {
                if let Some(icon_data) = icon {
                    let mut inner = inner.lock().unwrap();
                    let rgba = icon_data.data;
                    let data = rgba_to_argb32(&rgba);
                    inner.icon_pixmap = vec![IconPixmap {
                        width: icon_data.width,
                        height: icon_data.height,
                        data,
                    }];
                }
                if let Some(ref tooltip_val) = tooltip {
                    let mut inner = inner.lock().unwrap();
                    inner.tooltip = Some(tooltip_val.clone());
                }
                if let Some(ref title_val) = title {
                    let mut inner = inner.lock().unwrap();
                    inner.title = Some(title_val.clone());
                }
                {
                    let mut inner = inner.lock().unwrap();
                    inner.status = if visible {
                        "Active"
                    } else {
                        "Passive"
                    }
                    .to_string();
                }
                if let Some(ref menu_snapshot) = menu {
                    let mut inner = inner.lock().unwrap();
                    inner.revision += 1;
                    inner.menu = Some(menu_snapshot.clone());
                }

                if let (Some(sni_obj), Some(menu_obj)) = (
                    conn.object_server()
                        .interface::<_, StatusNotifierItem>(SNI_PATH)
                        .await
                        .ok(),
                    conn.object_server()
                        .interface::<_, DbusMenu>(MENU_PATH)
                        .await
                        .ok(),
                ) {
                    let _ = StatusNotifierItem::new_icon(&sni_obj.signal_emitter()).await;
                    let _ = StatusNotifierItem::new_tool_tip(&sni_obj.signal_emitter()).await;
                    let _ = StatusNotifierItem::new_title(&sni_obj.signal_emitter()).await;
                    let rev = {
                        let i = inner.lock().unwrap();
                        i.revision
                    };
                    let _ = DbusMenu::layout_updated(
                        &menu_obj.signal_emitter(),
                        rev,
                        0,
                    )
                    .await;
                }
            }
        }
    }
}
