#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarLifecycle {
    Idle,
    Warming,
    Ready,
    Closing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarCoordinatorState {
    pub mode: String,
    pub visible: bool,
    pub initializing: bool,
    pub init_label: String,
    pub width: u32,
    pub lifecycle: SidebarLifecycle,
}

#[derive(Debug, Clone)]
pub struct SidebarCoordinator {
    width: u32,
    visible: bool,
    hidden_by_user: bool,
    lifecycle: SidebarLifecycle,
    warmup_until: Option<u64>,
}

impl SidebarCoordinator {
    pub fn new(width: u32) -> Self {
        Self {
            width,
            visible: false,
            hidden_by_user: false,
            lifecycle: SidebarLifecycle::Idle,
            warmup_until: None,
        }
    }

    pub fn state(&self) -> SidebarCoordinatorState {
        let mode = match (self.visible, self.lifecycle) {
            (true, SidebarLifecycle::Closing) => "closing",
            (false, _) => "hidden",
            (true, SidebarLifecycle::Warming) => "warming",
            (true, SidebarLifecycle::Ready | SidebarLifecycle::Idle) => "ready",
        };
        let init_label = match mode {
            "warming" => "warming up…",
            "closing" => "closing…",
            _ => "",
        };

        SidebarCoordinatorState {
            mode: mode.to_string(),
            visible: self.visible,
            initializing: !init_label.is_empty(),
            init_label: init_label.to_string(),
            width: self.width,
            lifecycle: self.lifecycle,
        }
    }

    pub fn set_width(&mut self, width: u32) {
        self.width = width;
    }

    pub fn begin_warmup(&mut self) {
        if self.is_closing() {
            return;
        }
        self.visible = true;
        self.hidden_by_user = false;
        self.lifecycle = SidebarLifecycle::Warming;
        self.warmup_until = None;
    }

    pub fn begin_warmup_until(&mut self, until: u64) {
        self.begin_warmup();
        self.warmup_until = Some(until);
    }

    pub fn warmup_done(&mut self) {
        if self.is_closing() || self.hidden_by_user {
            return;
        }
        self.visible = true;
        self.lifecycle = SidebarLifecycle::Ready;
        self.warmup_until = None;
    }

    pub fn mark_ready(&mut self) {
        self.warmup_done();
    }

    pub fn acknowledge_sidebar_connected(&mut self) {
        if self.is_closing() || self.hidden_by_user {
            return;
        }
        self.visible = true;
        if self.lifecycle != SidebarLifecycle::Warming {
            self.lifecycle = SidebarLifecycle::Ready;
        }
    }

    pub fn hide(&mut self) {
        if self.is_closing() {
            return;
        }
        self.visible = false;
        self.hidden_by_user = true;
        self.lifecycle = SidebarLifecycle::Idle;
        self.warmup_until = None;
    }

    pub fn begin_closing(&mut self) {
        self.visible = true;
        self.lifecycle = SidebarLifecycle::Closing;
        self.warmup_until = None;
    }

    pub fn tick_timers(&mut self, now: u64) -> bool {
        let before = self.state();
        if self.lifecycle == SidebarLifecycle::Warming
            && self.warmup_until.is_some_and(|until| now >= until)
        {
            self.lifecycle = SidebarLifecycle::Ready;
            self.warmup_until = None;
        }
        before != self.state()
    }

    fn is_closing(&self) -> bool {
        self.lifecycle == SidebarLifecycle::Closing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_connection_cannot_undo_an_explicit_hide() {
        let mut coordinator = SidebarCoordinator::new(36);
        coordinator.acknowledge_sidebar_connected();
        coordinator.hide();

        coordinator.acknowledge_sidebar_connected();
        coordinator.mark_ready();

        assert!(!coordinator.state().visible);
    }

    #[test]
    fn showing_again_accepts_sidebar_connections() {
        let mut coordinator = SidebarCoordinator::new(36);
        coordinator.hide();
        coordinator.begin_warmup();

        coordinator.acknowledge_sidebar_connected();

        assert!(coordinator.state().visible);
    }
}
