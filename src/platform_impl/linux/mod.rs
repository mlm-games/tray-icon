mod icon;
mod service;

use std::path::Path;
use std::thread;

use crossbeam_channel::unbounded;

use crate::icon::Icon;
use crate::menu::{ContextMenu, MenuItemKind};
pub(crate) use icon::PlatformIcon;
use service::{Command, IconData, MenuItemNode, MenuSnapshot};

use crate::{TrayIconAttributes, TrayIconId};

pub struct TrayIcon {
    id: TrayIconId,
    cmd_tx: crossbeam_channel::Sender<Command>,
    menu: Option<Box<dyn ContextMenu>>,
}

impl TrayIcon {
    pub fn new(id: TrayIconId, attrs: TrayIconAttributes) -> crate::Result<Self> {
        let (cmd_tx, cmd_rx) = unbounded();

        let tray_id = id.clone();
        let init_icon = attrs.icon.as_ref().map(|i| IconData {
            width: i.inner.width,
            height: i.inner.height,
            data: i.inner.rgba.clone(),
        });
        let init_tooltip = attrs.tooltip.clone();
        let init_title = attrs.title.clone();
        thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("tray-icon: failed to create tokio runtime: {e}");
                    return;
                }
            };
            rt.block_on(service::run_service(
                cmd_rx,
                tray_id,
                init_icon,
                init_tooltip,
                init_title,
            ));
        });

        let mut tray = Self {
            id,
            cmd_tx,
            menu: None,
        };

        let icon = attrs.icon.map(|i| IconData {
            width: i.inner.width,
            height: i.inner.height,
            data: i.inner.rgba,
        });

        if let Some(menu) = attrs.menu {
            tray.menu = Some(menu);
        }

        let snapshot = tray.current_menu_snapshot();

        let _ = tray.cmd_tx.send(Command::Update {
            id: tray.id.clone(),
            icon,
            tooltip: attrs.tooltip,
            title: attrs.title,
            visible: true,
            menu: snapshot,
        });

        Ok(tray)
    }

    fn current_menu_snapshot(&self) -> Option<MenuSnapshot> {
        let menu = self.menu.as_ref()?;
        let menu = menu.as_menu()?;
        let items = build_menu_tree(menu.items());
        Some(MenuSnapshot { items })
    }

    pub fn set_icon(&mut self, icon: Option<Icon>) -> crate::Result<()> {
        let icon = icon.map(|i| IconData {
            width: i.inner.width,
            height: i.inner.height,
            data: i.inner.rgba,
        });

        let _ = self.cmd_tx.send(Command::Update {
            id: self.id.clone(),
            icon,
            tooltip: None,
            title: None,
            visible: true,
            menu: None,
        });

        Ok(())
    }

    pub fn set_menu(&mut self, menu: Option<Box<dyn ContextMenu>>) {
        self.menu = menu;
        let snapshot = self.current_menu_snapshot();

        let _ = self.cmd_tx.send(Command::Update {
            id: self.id.clone(),
            icon: None,
            tooltip: None,
            title: None,
            visible: true,
            menu: snapshot,
        });
    }

    pub fn set_tooltip<S: AsRef<str>>(&mut self, tooltip: Option<S>) -> crate::Result<()> {
        let _ = self.cmd_tx.send(Command::Update {
            id: self.id.clone(),
            icon: None,
            tooltip: tooltip.map(|s| s.as_ref().to_string()),
            title: None,
            visible: true,
            menu: None,
        });
        Ok(())
    }

    pub fn set_title<S: AsRef<str>>(&mut self, title: Option<S>) {
        let _ = self.cmd_tx.send(Command::Update {
            id: self.id.clone(),
            icon: None,
            tooltip: None,
            title: title.map(|s| s.as_ref().to_string()),
            visible: true,
            menu: None,
        });
    }

    pub fn set_visible(&mut self, visible: bool) -> crate::Result<()> {
        let _ = self.cmd_tx.send(Command::Update {
            id: self.id.clone(),
            icon: None,
            tooltip: None,
            title: None,
            visible,
            menu: None,
        });
        Ok(())
    }

    pub fn set_temp_dir_path<P: AsRef<Path>>(&mut self, _path: Option<P>) {}

    pub fn rect(&self) -> Option<crate::Rect> {
        None
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Command::Shutdown);
    }
}

fn build_menu_tree(items: Vec<MenuItemKind>) -> Vec<MenuItemNode> {
    items
        .into_iter()
        .map(|item| match item {
            MenuItemKind::MenuItem(item) => MenuItemNode {
                id: item.id().clone(),
                label: item.text().to_string(),
                enabled: item.is_enabled(),
                checked: None,
                is_separator: false,
                is_submenu: false,
                children: Vec::new(),
            },
            MenuItemKind::Check(item) => MenuItemNode {
                id: item.id().clone(),
                label: item.text().to_string(),
                enabled: item.is_enabled(),
                checked: Some(item.is_checked()),
                is_separator: false,
                is_submenu: false,
                children: Vec::new(),
            },
            MenuItemKind::Icon(item) => MenuItemNode {
                id: item.id().clone(),
                label: item.text().to_string(),
                enabled: item.is_enabled(),
                checked: None,
                is_separator: false,
                is_submenu: false,
                children: Vec::new(),
            },
            MenuItemKind::Predefined(item) => MenuItemNode {
                id: item.id().clone(),
                label: item.text().to_string(),
                enabled: true,
                checked: None,
                is_separator: item.text().is_empty(),
                is_submenu: false,
                children: Vec::new(),
            },
            MenuItemKind::Submenu(item) => MenuItemNode {
                id: item.id().clone(),
                label: item.text().to_string(),
                enabled: item.is_enabled(),
                checked: None,
                is_separator: false,
                is_submenu: true,
                children: build_menu_tree(item.items()),
            },
        })
        .collect()
}
