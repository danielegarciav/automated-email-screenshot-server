use std::cell::OnceCell;
use std::marker::PhantomData;
use std::sync::{Condvar, Mutex, OnceLock};
use std::thread_local;
use std::time::Duration;

use uiautomation::actions::Scroll;
use uiautomation::controls::DocumentControl;
use uiautomation::variants::Variant;
use uiautomation::{UIAutomation, UIElement};

use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
  IUIAutomation, IUIAutomationElement, IUIAutomationPropertyChangedEventHandler,
  IUIAutomationPropertyChangedEventHandler_Impl, TreeScope_Element, UIA_ScrollPatternNoScroll as NoScroll,
  UIA_ScrollVerticalScrollPercentPropertyId, UIA_PROPERTY_ID,
};

/// The scroll sync channel, of which there's a single instance shared between our thread and the scroll event handler
/// thread, is used by the main (scroll sender) thread to wait until a scroll has finished, and the by the scroll event
/// handler thread to signal that a scroll has finished.
static SCROLL_SYNC_CHANNEL: OnceLock<SyncChannel> = OnceLock::new();

thread_local! {
  /// The scroll event handler, of which there's a single instance for the main thread, is a COM object passed on to
  /// the Window's native API, which will execute the callback (from a different thread) once a scroll event occurs.
  /// It will notify the main (scroll sender) thread when a scroll has finished.
  static SCROLL_EVENT_HANDLER: OnceCell<IUIAutomationPropertyChangedEventHandler> = OnceCell::new();
}

#[windows_implement::implement(IUIAutomationPropertyChangedEventHandler)]
struct ScrollEventHandler;

impl IUIAutomationPropertyChangedEventHandler_Impl for ScrollEventHandler {
  #[allow(non_snake_case)]
  fn HandlePropertyChangedEvent(
    &self,
    _sender: Option<&IUIAutomationElement>,
    propertyid: UIA_PROPERTY_ID,
    newvalue: &VARIANT,
  ) -> ::windows::core::Result<()> {
    // wrap around native windows variant
    let new_value_variant: Variant = newvalue.clone().into();
    let new_scroll_value: f64 = new_value_variant.try_into().unwrap();

    let element: UIElement = _sender.unwrap().clone().into();
    let pid = element.get_process_id().unwrap();

    SCROLL_SYNC_CHANNEL.get().unwrap().notify();

    tracing::debug!(
      "scroll event - new value: {}, prop id: {:?}, sender: {:?}, pid: {}",
      new_scroll_value / 100.0,
      propertyid.0,
      element,
      pid,
    );
    windows::core::Result::Ok(())
  }
}

struct SyncChannel {
  is_waiting: Mutex<bool>,
  cond_var: Condvar,
}

impl SyncChannel {
  fn new() -> Self {
    Self {
      is_waiting: Mutex::new(false),
      cond_var: Condvar::new(),
    }
  }
  fn wait(&self) {
    *self.is_waiting.lock().unwrap() = true;
    let (_guard, result) = self
      .cond_var
      .wait_timeout_while(
        self.is_waiting.lock().unwrap(),
        Duration::from_millis(10_000),
        |is_waiting| *is_waiting,
      )
      .unwrap();

    if result.timed_out() {
      tracing::warn!("waiting timed out! (10 seconds)");
    }
  }

  fn notify(&self) {
    let mut is_waiting = self.is_waiting.lock().unwrap();
    *is_waiting = false;
    self.cond_var.notify_all();
  }
}

pub struct ViewportScroller<'a> {
  // Pretend like we store a shared reference to the automation and element objects. This tells the compiler that
  // a `ViewportScroller` should not outlive the references passed on to `new()`. Otherwise, a `ViewportScroller`
  // could accidentally prevent their backing memory from being released long after they're no longer used.
  phantom: PhantomData<(&'a UIAutomation, &'a UIElement)>,
  win_automation: IUIAutomation,
  win_element: IUIAutomationElement,
  viewport_control: DocumentControl,
}

impl<'a> ViewportScroller<'a> {
  pub fn new(automation: &'a UIAutomation, viewport_element: &'a UIElement) -> Self {
    let win_automation: IUIAutomation = automation.clone().into();
    let win_element: IUIAutomationElement = viewport_element.clone().into();
    let viewport_control: DocumentControl = viewport_element.clone().try_into().unwrap();
    SCROLL_SYNC_CHANNEL.get_or_init(SyncChannel::new);
    SCROLL_EVENT_HANDLER.with(|scroll_event_handler| unsafe {
      win_automation
        .AddPropertyChangedEventHandlerNativeArray(
          &win_element,
          TreeScope_Element,
          None,
          scroll_event_handler.get_or_init(|| ScrollEventHandler.into()),
          &[UIA_ScrollVerticalScrollPercentPropertyId],
        )
        .unwrap();
    });
    Self {
      phantom: PhantomData,
      win_automation,
      win_element,
      viewport_control,
    }
  }

  pub fn scroll_to(&self, position: f64) -> anyhow::Result<()> {
    // TODO: lock the sync channel from getting notified until we start waiting
    // Currently there's a highly unlikely but non-zero chance that the scroll finishes before we start
    // waiting for a signal, which would lead to a deadlock.
    self.viewport_control.set_scroll_percent(NoScroll, position)?;
    SCROLL_SYNC_CHANNEL.get().unwrap().wait();
    Ok(())
  }
}

impl<'a> Drop for ViewportScroller<'a> {
  fn drop(&mut self) {
    SCROLL_EVENT_HANDLER.with(|scroll_event_handler| unsafe {
      self
        .win_automation
        .RemovePropertyChangedEventHandler(&self.win_element, scroll_event_handler.get().unwrap())
        .unwrap();
    });
  }
}
