//! Run by the main-thread macos_native harness, not a libtest worker.
use super::*;

pub(super) fn verify_editing(desktop: &EventLoop) {
    let menu = desktop.app.mainMenu().expect("standard editing menu");
    let edit = menu.itemAtIndex(1).unwrap().submenu().unwrap();
    assert!(edit.autoenablesItems());
    for (index, (action, key, shift)) in [
        (sel!(undo:), "z", false),
        (sel!(redo:), "z", true),
        (sel!(cut:), "x", false),
        (sel!(copy:), "c", false),
        (sel!(paste:), "v", false),
        (sel!(selectAll:), "a", false),
    ]
    .into_iter()
    .enumerate()
    {
        let item = edit.itemAtIndex(index as _).unwrap();
        assert_eq!(item.action(), Some(action));
        assert_eq!(item.keyEquivalent().to_string(), key);
        assert_eq!(
            item.keyEquivalentModifierMask(),
            NSEventModifierFlags::Command
                | if shift {
                    NSEventModifierFlags::Shift
                } else {
                    NSEventModifierFlags::empty()
                }
        );
        assert!(item.target().is_none());
    }

    let replies = Arc::new(Mutex::new(Vec::new()));
    let output = Arc::clone(&replies);
    let window = Window::new(
        desktop.mtm,
        Request {
            id: "editing".into(),
            agent: "DeepSeek Harness".into(),
            agent_path: Some(
                "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness".into(),
            ),
            reason: "Read-only request".into(),
            scope: "current user".into(),
            secret_keys: vec![],
            deadline: Instant::now() + std::time::Duration::from_secs(60),
            complete: Box::new(move |outcome| output.lock().unwrap().push(outcome)),
        },
    );
    window.window.makeKeyAndOrderFront(None);
    let note = &window.delegate.ivars().note;
    assert!(window.window.makeFirstResponder(Some(note)));
    assert!(window.delegate.ivars().remember.isEnabled());
    assert!(note.allowsUndo());
    note.setString(ns_string!("备注 🧁\nsecond line"));
    note.setSelectedRange(NSRange::new(0, 0));
    send_key(desktop, &window, "a", 0, NSEventModifierFlags::Command);
    assert_eq!(note.selectedRange(), NSRange::new(0, note.string().len()));

    // Real NSTextView edits exercise the responder-chain undo/redo actions.
    // No clipboard content is read or overwritten by this test.
    let undo = note.undoManager().unwrap();
    undo.removeAllActions();
    undo.setGroupsByEvent(false);
    undo.beginUndoGrouping();
    send_key(desktop, &window, "r", 15, NSEventModifierFlags::empty());
    undo.endUndoGrouping();
    assert_eq!(note.string().to_string(), "r");
    assert!(undo.canUndo());
    send_key(desktop, &window, "z", 6, NSEventModifierFlags::Command);
    assert_eq!(note.string().to_string(), "备注 🧁\nsecond line");
    assert!(undo.canRedo());
    send_key(
        desktop,
        &window,
        "Z",
        6,
        NSEventModifierFlags::Command | NSEventModifierFlags::Shift,
    );
    assert_eq!(note.string().to_string(), "r");

    // Native selection still works in a read-only field, but edits are disabled.
    let (readonly_scroll, readonly) = text_area(desktop.mtm, "read only", false, 52.0);
    window
        .window
        .contentView()
        .unwrap()
        .addSubview(&readonly_scroll);
    assert!(window.window.makeFirstResponder(Some(&readonly)));
    send_key(desktop, &window, "a", 0, NSEventModifierFlags::Command);
    assert_eq!(
        readonly.selectedRange(),
        NSRange::new(0, readonly.string().len())
    );
    assert!(!readonly.allowsUndo());
    edit.update();
    assert!(!edit.itemAtIndex(2).unwrap().isEnabled()); // Cut
    assert!(edit.itemAtIndex(3).unwrap().isEnabled()); // Copy
    assert!(!edit.itemAtIndex(4).unwrap().isEnabled()); // Paste

    // Menu shortcuts must not create keyboard shortcuts for consent decisions.
    let button: Retained<DecisionButton> = unsafe {
        msg_send![DecisionButton::alloc(desktop.mtm), initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(84.0, 32.0))]
    };
    unsafe {
        button.setTarget(Some(&window.delegate));
        button.setAction(Some(sel!(accept:)));
    }
    for (key, code) in [("\r", 36), (" ", 49), ("\u{1b}", 53), ("\r", 76)] {
        let event = key_event(&window, key, code, NSEventModifierFlags::empty());
        assert!(!button.performKeyEquivalent(&event));
        button.keyDown(&event);
    }
    assert!(replies.lock().unwrap().is_empty());
    assert!(window.delegate.ivars().complete.borrow().is_some());
}

fn send_key(
    desktop: &EventLoop,
    window: &Window,
    key: &str,
    code: u16,
    flags: NSEventModifierFlags,
) {
    desktop.app.sendEvent(&key_event(window, key, code, flags));
}

fn key_event(
    window: &Window,
    key: &str,
    code: u16,
    flags: NSEventModifierFlags,
) -> Retained<NSEvent> {
    NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
        NSEventType::KeyDown,
        NSPoint::new(0.0, 0.0),
        flags,
        0.0,
        window.window.windowNumber(),
        None,
        &NSString::from_str(key),
        &NSString::from_str(key),
        false,
        code,
    ).unwrap()
}
