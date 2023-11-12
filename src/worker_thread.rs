mod viewport_scroller;

use anyhow::Context;
use itertools::Itertools;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use image::{DynamicImage, RgbaImage};
use win_screenshot::capture::capture_window_ex;

use uiautomation::actions::{Scroll, Transform, Window};
use uiautomation::controls::{ControlType, DocumentControl, WindowControl};
use uiautomation::UIAutomation;
use viewport_scroller::ViewportScroller;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::HiDpi;

use crate::eml_task::{EmlTaskManager, EmlTaskStatus};

fn init_dpi_awareness() -> anyhow::Result<()> {
  unsafe {
    HiDpi::SetProcessDpiAwarenessContext(HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)?;
    thread::sleep(Duration::from_millis(30));
  };
  Ok(())
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

  loop {
    tracing::info!("waiting for next task...");
    match task_manager.receive_next_task() {
      None => {
        tracing::info!("task channel closed, exiting thread...");
        return Ok(());
      }
      Some(task) => {
        let _span_guard = task.span.map(|span| span.entered());
        let response = task.response;
        std::fs::write(&eml_file_path, task.eml_content).unwrap();
        let result = perform_email_screenshot(&automation, &eml_file_path);
        match result {
          Ok(..) => task_manager.report_task_status(&task.id, EmlTaskStatus::Saving),
          Err(..) => task_manager.report_task_status(&task.id, EmlTaskStatus::Failed),
        }
        response.send(result).unwrap();
      }
    }
    thread::sleep(Duration::from_millis(1000));
  }
}

fn close_existing_windows(automation: &UIAutomation) -> anyhow::Result<()> {
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
      .flat_map(|pid| [String::from("/pid"), pid.to_string()])
      .collect();
    taskkill_args.push(String::from("/f"));
    if let Err(err) = Command::new("taskkill.exe").args(taskkill_args).status() {
      tracing::warn!("failed to terminate existing windows, {:?}", err);
    }
  }
  Ok(())
}

fn perform_email_screenshot(
  automation: &UIAutomation,
  eml_file_path: &PathBuf,
) -> anyhow::Result<Vec<DynamicImage>> {
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
  // some versions of Windows Mail won't interact with automation APIs until queried
  // for interaction state, even if the query fails.
  if outer_window_control.get_window_interaction_state().is_err() {
    tracing::warn!("failed to get window interaction state");
  }
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

  let viewport_rect = viewport_element.get_bounding_rectangle()?;

  let viewport_top = viewport_rect.get_top() - window_rect.get_top();
  let viewport_bottom = viewport_rect.get_bottom() - window_rect.get_top();
  let viewport_height = viewport_bottom - viewport_top;

  let viewport_left = viewport_rect.get_left() - window_rect.get_left();
  let viewport_right = viewport_rect.get_right() - window_rect.get_left();
  let viewport_width = viewport_right - viewport_left;

  let viewport_control: DocumentControl = viewport_element.clone().try_into()?;
  let viewport_height_percentage = viewport_control.get_vertical_view_size()? / 100.0;
  let document_height = f64::round(viewport_height as f64 / viewport_height_percentage) as i32;
  let max_scroll_height = document_height - viewport_height;
  tracing::debug!(viewport_height, document_height, max_scroll_height);

  let viewport_scroller = ViewportScroller::new(automation, &viewport_element);
  if viewport_control.get_vertical_scroll_percent()? > 0.0 && viewport_height_percentage < 1.0 {
    tracing::info!("rewinding viewport to top...");
    viewport_scroller.scroll_to(0.0)?;
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
    viewport_scroller.scroll_to(next_scroll_percent * 100.0)?;
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

  drop(viewport_scroller);
  close_existing_windows(automation)?;
  Ok(captures)
}
