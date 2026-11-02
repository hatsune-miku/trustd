//! AppKit objects never leave the main thread. Worker threads submit owned
//! Rust messages; policy and authorization remain in the daemon.
use std::{
    cell::RefCell,
    collections::HashMap,
    io,
    sync::{Arc, Mutex, mpsc},
    time::Instant,
};

use objc2::{
    DefinedClass, MainThreadOnly, define_class, msg_send,
    rc::{Retained, autoreleasepool},
    runtime::ProtocolObject,
    sel,
};
use objc2_app_kit::*;
use objc2_foundation::{
    MainThreadMarker, NSDate, NSDefaultRunLoopMode, NSNotification, NSObject, NSObjectProtocol,
    NSPoint, NSRange, NSRect, NSSize, NSString, ns_string,
};

use crate::{Choice, Event, Outcome, Request, types::Completion};

const MAX_WINDOWS: usize = 8;
const NOTE_LIMIT: usize = 4096;

struct Mailbox {
    queue: mpsc::SyncSender<Event>,
    stop: Option<i32>,
}

#[derive(Clone)]
pub struct Sender(Arc<Mutex<Option<Mailbox>>>);

impl Sender {
    pub fn send(&self, event: Event) -> io::Result<()> {
        let mut mailbox = self
            .0
            .lock()
            .map_err(|_| io::Error::other("UI mailbox poisoned"))?;
        let mailbox = mailbox
            .as_mut()
            .ok_or_else(|| io::Error::other("UI stopped"))?;
        if let Event::Stop(code) = event {
            mailbox.stop = Some(code);
            Ok(())
        } else {
            mailbox
                .queue
                .try_send(event)
                .map_err(|_| io::Error::other("UI queue unavailable"))
        }
    }
}

pub struct EventLoop {
    app: Retained<NSApplication>,
    mtm: MainThreadMarker,
    receiver: mpsc::Receiver<Event>,
    sender: Sender,
    windows: HashMap<String, Window>,
}

impl EventLoop {
    pub fn new() -> io::Result<Self> {
        let mtm = MainThreadMarker::new()
            .ok_or_else(|| io::Error::other("AppKit requires the main thread"))?;
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        app.finishLaunching();
        let (queue, receiver) = mpsc::sync_channel(64);
        Ok(Self {
            app,
            mtm,
            receiver,
            sender: Sender(Arc::new(Mutex::new(Some(Mailbox { queue, stop: None })))),
            windows: HashMap::new(),
        })
    }

    pub fn sender(&self) -> Sender {
        self.sender.clone()
    }

    pub fn run(mut self) -> i32 {
        loop {
            let stop = self
                .sender
                .0
                .lock()
                .ok()
                .and_then(|state| state.as_ref().and_then(|state| state.stop));
            if let Some(code) = stop {
                return code;
            }
            while let Ok(event) = self.receiver.try_recv() {
                match event {
                    Event::Show(request) => {
                        if request.deadline <= Instant::now() || self.windows.len() >= MAX_WINDOWS {
                            continue;
                        }
                        let id = request.id.clone();
                        // Drop an old same-ID window before installing its replacement.
                        self.windows.remove(&id);
                        let window = Window::new(self.mtm, request);
                        window.window.makeKeyAndOrderFront(None);
                        #[allow(deprecated)]
                        self.app.activateIgnoringOtherApps(true);
                        self.windows.insert(id, window);
                    }
                    Event::Close(id) => {
                        self.windows.remove(&id);
                    }
                    Event::Stop(code) => return code,
                }
            }
            self.windows.retain(|_, window| {
                let state = window.delegate.ivars();
                if Instant::now() >= state.deadline {
                    window.delegate.finish(Choice::Ignore);
                }
                if state.complete.borrow().is_none() {
                    return false;
                }
                let seconds = state
                    .deadline
                    .saturating_duration_since(Instant::now())
                    .as_secs()
                    .saturating_add(1);
                window
                    .countdown
                    .setStringValue(&NSString::from_str(&format!("剩余 {seconds} 秒")));
                true
            });
            autoreleasepool(|_| {
                // A bounded native event wait also services the Rust mailbox
                // and expiry without any worker-thread access to AppKit.
                let until = NSDate::dateWithTimeIntervalSinceNow(0.1);
                let mode = unsafe { NSDefaultRunLoopMode };
                if let Some(event) = self.app.nextEventMatchingMask_untilDate_inMode_dequeue(
                    NSEventMask::Any,
                    Some(&until),
                    mode,
                    true,
                ) {
                    self.app.sendEvent(&event);
                }
                self.app.updateWindows();
            });
        }
    }
}

impl Drop for EventLoop {
    fn drop(&mut self) {
        if let Ok(mut mailbox) = self.sender.0.lock() {
            mailbox.take();
        }
        self.windows.clear();
    }
}

struct State {
    deadline: Instant,
    complete: RefCell<Option<Completion>>,
    note: Retained<NSTextView>,
    remember: Retained<NSButton>,
}

define_class!(
    // SAFETY: NSObject has no extra subclass requirements. State is main-thread-only.
    #[unsafe(super = NSObject)]
    #[name = "TrustdConsentDelegate"]
    #[thread_kind = MainThreadOnly]
    #[ivars = State]
    struct Delegate;
    unsafe impl NSObjectProtocol for Delegate {}
    unsafe impl NSWindowDelegate for Delegate {
        #[unsafe(method(windowWillClose:))]
        fn closed(&self, _: &NSNotification) { self.finish(Choice::Ignore); }
    }
    unsafe impl NSTextDelegate for Delegate {}
    unsafe impl NSTextViewDelegate for Delegate {
        #[unsafe(method(textView:shouldChangeTextInRange:replacementString:))]
        fn note_change(&self, text: &NSTextView, range: NSRange, replacement: Option<&NSString>) -> bool {
            replacement.is_none_or(|replacement| {
                text.string().len().saturating_sub(range.length).saturating_add(replacement.len()) <= NOTE_LIMIT
            })
        }
    }
    impl Delegate {
        #[unsafe(method(accept:))]
        fn accept(&self, _: &NSButton) { self.finish(Choice::Accept); }
        #[unsafe(method(deny:))]
        fn deny(&self, _: &NSButton) { self.finish(Choice::Deny); }
        #[unsafe(method(ignore:))]
        fn ignore(&self, _: &NSButton) { self.finish(Choice::Ignore); }
    }
);

impl Delegate {
    fn new(mtm: MainThreadMarker, state: State) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(state);
        unsafe { msg_send![super(this), init] }
    }
    fn finish(&self, choice: Choice) {
        let state = self.ivars();
        let complete = state.complete.borrow_mut().take();
        if let Some(complete) = complete {
            let choice = if Instant::now() >= state.deadline {
                Choice::Ignore
            } else {
                choice
            };
            complete(Outcome {
                choice,
                remember: choice != Choice::Ignore
                    && state.remember.state() == NSControlStateValueOn,
                note: state
                    .note
                    .string()
                    .to_string()
                    .chars()
                    .take(NOTE_LIMIT)
                    .collect(),
            });
        }
    }
}

define_class!(
    // SAFETY: NSButton has no extra subclass requirements; no ivars are added.
    #[unsafe(super = NSButton)]
    #[name = "TrustdConsentDecisionButton"]
    #[thread_kind = MainThreadOnly]
    struct DecisionButton;
    unsafe impl NSObjectProtocol for DecisionButton {}
    impl DecisionButton {
        // Do not submit from Space/Return/Escape or single-letter shortcuts.
        // Keyboard traversal and editing remain on native text controls.
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if !matches!(event.keyCode(), 36 | 49 | 53 | 76) {
                unsafe { let _: () = msg_send![super(self), keyDown: event]; }
            }
        }
        #[unsafe(method(performKeyEquivalent:))]
        fn key_equivalent(&self, _: &NSEvent) -> bool { false }
    }
);

struct Window {
    window: Retained<NSWindow>,
    delegate: Retained<Delegate>,
    countdown: Retained<NSTextField>,
}

impl Window {
    fn new(mtm: MainThreadMarker, request: Request) -> Self {
        // SAFETY: all AppKit operations run on the main thread. Retained owns
        // each object and auto-release-on-close is disabled for the window.
        unsafe {
            let has_secrets = !request.secret_keys.is_empty();
            let size = NSSize::new(600.0, if has_secrets { 620.0 } else { 510.0 });
            let window = NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), size),
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Resizable,
                NSBackingStoreType::Buffered,
                false,
            );
            window.setReleasedWhenClosed(false);
            window.setTitle(ns_string!("trustd"));
            window.setContentMinSize(size);
            window.setLevel(NSFloatingWindowLevel);
            let content = window.contentView().expect("AppKit content view");
            let stack = NSStackView::new(mtm);
            stack.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
            stack.setAlignment(NSLayoutAttribute::Leading);
            stack.setSpacing(8.0);
            stack.setTranslatesAutoresizingMaskIntoConstraints(false);
            content.addSubview(&stack);
            stack
                .leadingAnchor()
                .constraintEqualToAnchor_constant(&content.leadingAnchor(), 20.0)
                .setActive(true);
            stack
                .trailingAnchor()
                .constraintEqualToAnchor_constant(&content.trailingAnchor(), -20.0)
                .setActive(true);
            stack
                .topAnchor()
                .constraintEqualToAnchor_constant(&content.topAnchor(), 20.0)
                .setActive(true);
            stack
                .bottomAnchor()
                .constraintLessThanOrEqualToAnchor_constant(&content.bottomAnchor(), -20.0)
                .setActive(true);
            let title = label(mtm, "授权请求");
            title.setFont(Some(&NSFont::boldSystemFontOfSize(18.0)));
            add_full_width(&stack, &title);
            let countdown = label(mtm, "");
            add_full_width(&stack, &countdown);
            add_full_width(&stack, &label(mtm, &format!("来自 {}", request.agent)));
            add_full_width(&stack, &label(mtm, &format!("身份：{}", request.scope)));
            add_full_width(&stack, &label(mtm, "执行缘由"));
            let (reason, _) = text_area(mtm, &request.reason, false, 100.0);
            add_full_width(&stack, &reason);
            if has_secrets {
                add_full_width(
                    &stack,
                    &label(mtm, &format!("凭据（{}）", request.secret_keys.len())),
                );
                let (secrets, _) = text_area(mtm, &request.secret_keys.join("\n"), false, 72.0);
                add_full_width(&stack, &secrets);
            }
            add_full_width(&stack, &label(mtm, "附加说明"));
            let (note_scroll, note) = text_area(mtm, "", true, 100.0);
            add_full_width(&stack, &note_scroll);
            let remember = NSButton::checkboxWithTitle_target_action(
                ns_string!("记住本次选择"),
                None,
                None,
                mtm,
            );
            stack.addArrangedSubview(&remember);
            let delegate = Delegate::new(
                mtm,
                State {
                    deadline: request.deadline,
                    complete: RefCell::new(Some(request.complete)),
                    note,
                    remember,
                },
            );
            let buttons = NSStackView::new(mtm);
            buttons.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
            buttons.setSpacing(12.0);
            for (title, action) in [
                ("忽略", sel!(ignore:)),
                ("拒绝", sel!(deny:)),
                ("允许", sel!(accept:)),
            ] {
                let button: Retained<DecisionButton> = msg_send![DecisionButton::alloc(mtm), initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(84.0, 32.0))];
                button.setTitle(&NSString::from_str(title));
                button.setBezelStyle(NSBezelStyle::Push);
                button.setKeyEquivalent(ns_string!(""));
                button.setTarget(Some(&delegate));
                button.setAction(Some(action));
                button
                    .widthAnchor()
                    .constraintGreaterThanOrEqualToConstant(84.0)
                    .setActive(true);
                buttons.addArrangedSubview(&button);
            }
            stack.addArrangedSubview(&buttons);
            buttons
                .trailingAnchor()
                .constraintEqualToAnchor(&stack.trailingAnchor())
                .setActive(true);
            window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            delegate
                .ivars()
                .note
                .setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            window.makeFirstResponder(Some(&delegate.ivars().note));
            window.center();
            Self {
                window,
                delegate,
                countdown,
            }
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // External cancellation drops the one-shot callback without inventing
        // a user decision. The delegate outlives its window's close operation.
        self.delegate.ivars().complete.borrow_mut().take();
        self.delegate.ivars().note.setDelegate(None);
        self.window.setDelegate(None);
        self.window.close();
    }
}

fn label(mtm: MainThreadMarker, text: &str) -> Retained<NSTextField> {
    let label = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
    label.setSelectable(true);
    label
}

fn add_full_width(stack: &NSStackView, view: &NSView) {
    stack.addArrangedSubview(view);
    view.widthAnchor()
        .constraintEqualToAnchor(&stack.widthAnchor())
        .setActive(true);
}

fn text_area(
    mtm: MainThreadMarker,
    text: &str,
    editable: bool,
    height: f64,
) -> (Retained<NSScrollView>, Retained<NSTextView>) {
    unsafe {
        let scroll = NSScrollView::initWithFrame(
            NSScrollView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(560.0, height)),
        );
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setBorderType(NSBorderType::BezelBorder);
        scroll
            .heightAnchor()
            .constraintEqualToConstant(height)
            .setActive(true);
        let note = NSTextView::initWithFrame(
            NSTextView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), scroll.contentSize()),
        );
        note.setMinSize(NSSize::new(0.0, height));
        note.setMaxSize(NSSize::new(f64::MAX, f64::MAX));
        note.setVerticallyResizable(true);
        note.setHorizontallyResizable(false);
        note.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        if let Some(container) = note.textContainer() {
            container.setContainerSize(NSSize::new(560.0, f64::MAX));
            container.setWidthTracksTextView(true);
        }
        note.setRichText(false);
        note.setEditable(editable);
        note.setSelectable(true);
        note.setFont(Some(&NSFont::systemFontOfSize(13.0)));
        note.setTextContainerInset(NSSize::new(6.0, 6.0));
        note.setAutomaticQuoteSubstitutionEnabled(false);
        note.setAutomaticDashSubstitutionEnabled(false);
        note.setString(&NSString::from_str(text));
        scroll.setDocumentView(Some(&note));
        (scroll, note)
    }
}

#[cfg(test)]
#[allow(dead_code)]
pub(super) fn verify_native_controls() {
    use std::time::Duration;
    let desktop = EventLoop::new().unwrap();
    assert_eq!(
        desktop.app.activationPolicy(),
        NSApplicationActivationPolicy::Accessory
    );
    for choice in [Choice::Accept, Choice::Deny, Choice::Ignore] {
        let replies = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::clone(&replies);
        let request = Request {
            id: "native-test".into(),
            agent: "原生测试 🧁".into(),
            reason: "第一行\n完整缘由".repeat(300),
            scope: "当前用户与 root".into(),
            secret_keys: (0..64).map(|index| format!("CREDENTIAL_{index}")).collect(),
            deadline: Instant::now() + Duration::from_secs(30),
            complete: Box::new(move |outcome| output.lock().unwrap().push(outcome)),
        };
        let window = Window::new(desktop.mtm, request);
        window
            .delegate
            .ivars()
            .note
            .setString(&NSString::from_str("备注 A D I\n第二行 🧁"));
        window
            .delegate
            .ivars()
            .remember
            .setState(NSControlStateValueOn);
        window.delegate.finish(choice);
        window.delegate.finish(Choice::Accept);
        let replies = replies.lock().unwrap();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].choice, choice);
        assert_eq!(replies[0].remember, choice != Choice::Ignore);
        assert_eq!(replies[0].note, "备注 A D I\n第二行 🧁");
    }
    let replies = Arc::new(Mutex::new(Vec::new()));
    let output = Arc::clone(&replies);
    let expired = Window::new(
        desktop.mtm,
        Request {
            id: "expired".into(),
            agent: "late".into(),
            reason: "late".into(),
            scope: "root".into(),
            secret_keys: vec![],
            deadline: Instant::now(),
            complete: Box::new(move |outcome| output.lock().unwrap().push(outcome)),
        },
    );
    expired.delegate.finish(Choice::Accept);
    assert_eq!(replies.lock().unwrap()[0].choice, Choice::Ignore);
    assert!(!replies.lock().unwrap()[0].remember);
    let sender = desktop.sender();
    for index in 0..64 {
        sender.send(Event::Close(index.to_string())).unwrap();
    }
    sender.send(Event::Stop(7)).unwrap();
    assert_eq!(desktop.run(), 7);
    assert!(sender.send(Event::Close("closed".into())).is_err());
    println!("AppKit consent controls, deadlines, one-shot replies and mailbox lifecycle passed");
}
