#![allow(non_snake_case)]

use anyhow::Context;
use itertools::Itertools;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use image::{DynamicImage, RgbaImage};
use win_screenshot::capture::capture_window_ex;

use uiautomation::actions::{Scroll, Transform, Window};
use uiautomation::controls::{ControlType, DocumentControl, WindowControl};
use uiautomation::variants::Variant;
use uiautomation::{UIAutomation, UIElement};

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
  IUIAutomation, IUIAutomationElement, IUIAutomationPropertyChangedEventHandler,
  IUIAutomationPropertyChangedEventHandler_Impl, TreeScope_Element,
  UIA_ScrollPatternNoScroll as NoScroll, UIA_ScrollVerticalScrollPercentPropertyId,
  UIA_PROPERTY_ID,
};
use windows::Win32::UI::HiDpi;

use crate::eml_task::EmlTaskManager;

#[windows_implement::implement(IUIAutomationPropertyChangedEventHandler)]
struct ScrollEventHandler {
  scroll_sync_channel: Arc<SyncChannel>,
}

impl IUIAutomationPropertyChangedEventHandler_Impl for ScrollEventHandler {
  fn HandlePropertyChangedEvent(
    &self,
    _sender: Option<&IUIAutomationElement>,
    _propertyid: UIA_PROPERTY_ID,
    newvalue: &VARIANT,
  ) -> ::windows::core::Result<()> {
    // wrap around native windows variant
    let new_value_variant: Variant = newvalue.clone().into();
    let new_scroll_value: f64 = new_value_variant.try_into().unwrap();

    let el: UIElement = _sender.unwrap().clone().into();
    let pid = el.get_process_id().unwrap();

    self.scroll_sync_channel.notify();

    tracing::debug!(
      "scroll event: {}, prop id: {:?}, sender: {:?}, pid: {}",
      new_scroll_value,
      _propertyid,
      el,
      pid,
    );
    windows::core::Result::Ok(())
  }
}

fn init_dpi_awareness() -> anyhow::Result<()> {
  unsafe {
    HiDpi::SetProcessDpiAwarenessContext(HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)?;
    thread::sleep(Duration::from_millis(30));
  };
  Ok(())
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

pub fn start_worker_thread(task_manager: Arc<EmlTaskManager>) -> anyhow::Result<()> {
  init_dpi_awareness()?;

  let eml_directory = {
    let mut cwd = std::env::current_dir()?;
    cwd.push("eml");
    cwd
  };
  let eml_file_path = {
    let mut path = eml_directory.clone();
    path.push("currently-testing.eml");
    path
  };
  std::fs::create_dir_all(eml_directory)?;

  let automation = UIAutomation::new()?;
  let scroll_sync_channel = Arc::new(SyncChannel::new());
  let com_event_handler_alloc = ScrollEventHandler {
    scroll_sync_channel: scroll_sync_channel.clone(),
  };
  let com_event_handler: IUIAutomationPropertyChangedEventHandler = com_event_handler_alloc.into();

  loop {
    tracing::info!("receiving next task...");
    match task_manager.receive_next_task() {
      None => {
        tracing::warn!("task channel closed");
        return Ok(());
      }
      Some(task) => {
        let _span_guard = task.span.map(|span| span.entered());
        let response = task.response;
        std::fs::write(&eml_file_path, task.eml_content).unwrap();
        let result = perform_email_screenshot(
          &automation,
          &eml_file_path,
          &com_event_handler,
          &scroll_sync_channel,
        );
        match result {
          Ok(..) => task_manager.report_task_completion(&task.id, "test path".to_string()),
          Err(..) => task_manager.report_task_failure(&task.id),
        }
        response.send(result).unwrap();
      }
    }
    thread::sleep(Duration::from_millis(1000));
  }
}

fn close_existing_windows(automation: &UIAutomation) -> anyhow::Result<()> {
  tracing::info!("finding existing windows...");
  let existing_windows = automation
    .create_matcher()
    .name("Mail")
    .timeout(200)
    .control_type(ControlType::Window)
    .find_all()
    .or_else(|err| match err.code() {
      // timeout finished with no results
      2 => Ok(vec![]),
      _ => Err(err),
    })?;

  if !existing_windows.is_empty() {
    tracing::info!("terminating existing windows...");
    let mut taskkill_args: Vec<String> = existing_windows
      .into_iter()
      .filter_map(|window| window.get_process_id().ok())
      .unique()
      .flat_map(|pid| ["/pid".to_string(), pid.to_string()])
      .collect();
    taskkill_args.push("/f".to_string());
    if let Err(err) = Command::new("taskkill.exe").args(taskkill_args).status() {
      tracing::warn!("failed to terminate existing windows, {:?}", err);
    }
  }
  Ok(())
}

fn perform_email_screenshot(
  automation: &UIAutomation,
  eml_file_path: &PathBuf,
  scroll_event_handler: &IUIAutomationPropertyChangedEventHandler,
  scroll_sync_channel: &SyncChannel,
) -> anyhow::Result<DynamicImage> {
  close_existing_windows(automation)?;

  tracing::info!("launching mail app...");
  Command::new("explorer.exe").arg(eml_file_path).status()?;

  tracing::info!("finding outer window...");
  let outer_window_element_vec = automation
    .create_matcher()
    .control_type(ControlType::Window)
    .name("Mail")
    .timeout(10000)
    .find_all()?;

  let outer_window_element = outer_window_element_vec
    .last()
    .context("no outer windows found")?;

  tracing::info!("resizing outer window...");
  let outer_window_control: WindowControl = outer_window_element.try_into()?;
  if outer_window_control.get_window_interaction_state().is_err() {
    tracing::warn!("failed to get window interaction state");
  }
  let _ = dbg!(outer_window_control.get_window_interaction_state());
  // let _ = outer_window_control.set_foregrand()?;
  let _ = outer_window_control.normal();
  let _ = outer_window_control.resize(1600.0, 1432.0);
  thread::sleep(Duration::from_millis(1500));

  tracing::info!("finding inner window...");
  let window_element = automation
    .create_matcher()
    .name("Mail")
    .classname("Windows.UI.Core.CoreWindow")
    .control_type(ControlType::Window)
    .timeout(10000)
    .find_first()?;
  let window_handle: HWND = window_element.get_native_window_handle()?.into();
  let window_rect = window_element.get_bounding_rectangle()?;
  thread::sleep(Duration::from_millis(1500));

  let viewport_element = automation
    .create_matcher()
    .from(window_element)
    .control_type(ControlType::Document)
    .find_first()?;

  let a: IUIAutomation = automation.clone().into();
  let el: IUIAutomationElement = viewport_element.clone().into();
  unsafe {
    a.AddPropertyChangedEventHandlerNativeArray(
      &el,
      TreeScope_Element,
      None,
      scroll_event_handler,
      &[UIA_ScrollVerticalScrollPercentPropertyId],
    )
    .unwrap();
  }

  let viewport_rect = viewport_element.get_bounding_rectangle()?;

  let viewport_top = viewport_rect.get_top() - window_rect.get_top();
  let viewport_bottom = viewport_rect.get_bottom() - window_rect.get_top();
  let viewport_height = viewport_bottom - viewport_top;

  let viewport_left = viewport_rect.get_left() - window_rect.get_left();
  let viewport_right = viewport_rect.get_right() - window_rect.get_left();
  let viewport_width = viewport_right - viewport_left;

  let viewport_control: DocumentControl = viewport_element.try_into()?;
  let viewport_height_percentage = viewport_control.get_vertical_view_size()? / 100.0;
  let document_height = f64::round(viewport_height as f64 / viewport_height_percentage) as i32;
  let max_scroll_height = document_height - viewport_height;
  tracing::debug!(viewport_height, document_height, max_scroll_height);

  if viewport_control.get_vertical_scroll_percent()? > 0.0 && viewport_height_percentage < 1.0 {
    tracing::info!("rewinding viewport to top...");
    viewport_control.set_scroll_percent(NoScroll, 0.0)?;
    scroll_sync_channel.wait();
    thread::sleep(Duration::from_millis(500));
  }

  let bottom_margin = match document_height > viewport_height {
    true => 32,
    false => 0,
  };

  tracing::info!("taking initial screenshot...");
  let initial_capture: DynamicImage = {
    let buf = capture_window_ex(
      window_handle.0,
      win_screenshot::capture::Using::PrintWindow,
      win_screenshot::capture::Area::ClientOnly,
      Some([viewport_left, viewport_top]),
      Some([viewport_width, viewport_height - bottom_margin]),
    )?;
    RgbaImage::from_raw(buf.width, buf.height, buf.pixels)
      .context("could not create dynamic image")?
      .into()
  };

  let mut captures = vec![initial_capture];
  let mut scroll_percent = viewport_control.get_vertical_scroll_percent()? / 100.0;
  let mut scroll_height = 0;
  let mut screenshot_count = 1;

  loop {
    if viewport_height_percentage == 1.0 || scroll_percent >= 1.0 {
      break;
    }

    if screenshot_count >= 20 {
      tracing::warn!("max screenshot count reached");
      break;
    }

    let bottom_margin = 32;
    let next_scroll_increment = viewport_height - bottom_margin;
    let next_scroll_height = i32::min(max_scroll_height, scroll_height + next_scroll_increment);
    let next_scroll_percent = f64::min(1.0, next_scroll_height as f64 / max_scroll_height as f64);
    tracing::debug!(next_scroll_height, next_scroll_percent);

    tracing::info!("scrolling...");
    viewport_control.set_scroll_percent(NoScroll, next_scroll_percent * 100.0)?;
    scroll_sync_channel.wait();
    thread::sleep(Duration::from_millis(500));

    let scroll_difference = next_scroll_height - scroll_height;
    let overlapping_height = viewport_height - bottom_margin - scroll_difference;
    tracing::debug!(scroll_difference, overlapping_height);

    let is_last_screenshot = next_scroll_percent >= 1.0;

    let next_bottom_margin = match is_last_screenshot {
      false => bottom_margin,
      true => 0,
    };

    screenshot_count += 1;
    match is_last_screenshot {
      false => tracing::info!("taking screenshot #{screenshot_count}..."),
      true => tracing::info!("taking screenshot #{screenshot_count} (last screenshot)..."),
    };

    let capture: DynamicImage = {
      let buf = capture_window_ex(
        window_handle.0,
        win_screenshot::capture::Using::PrintWindow,
        win_screenshot::capture::Area::ClientOnly,
        Some([viewport_left, viewport_top + overlapping_height]),
        Some([
          viewport_width,
          viewport_height - overlapping_height - next_bottom_margin,
        ]),
      )?;
      RgbaImage::from_raw(buf.width, buf.height, buf.pixels)
        .context("could not create dynamic image")?
        .into()
    };
    captures.push(capture);

    scroll_percent = next_scroll_percent;
    scroll_height = next_scroll_height;

    if is_last_screenshot {
      break;
    }
  }

  tracing::info!("stitching...");
  let stitched_image = stitchy_core::Stitch::builder()
    .images(captures)
    .alignment(stitchy_core::AlignmentMode::Vertical)
    .height_limit(4096)
    .stitch()
    .map_err(|msg| anyhow::anyhow!(msg))?;

  unsafe { a.RemoveAllEventHandlers()? }
  tracing::info!("closing window now");
  close_existing_windows(automation)?;
  Ok(stitched_image)
}
