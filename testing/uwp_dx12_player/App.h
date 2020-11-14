/*
 * Copyright 2020 Google LLC
 *
 */

/*
 * Copyright (c) 2020, Alliance for Open Media. All rights reserved
 *
 * This source code is subject to the terms of the BSD 2 Clause License and
 * the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
 * was not distributed with this source code in the LICENSE file, you can
 * obtain it at www.aomedia.org/license/software. If the Alliance for Open
 * Media Patent License 1.0 was not distributed with this source code in the
 * PATENTS file, you can obtain it at www.aomedia.org/license/patent.
 */

#pragma once

#include "pch.h"
#include "dxengine.h"
#include "playerwnd.h"

namespace uwp_dx12_player {
// Main entry point for our app. Connects the app with the Windows shell and handles application lifecycle events.
ref class App sealed : public Windows::ApplicationModel::Core::IFrameworkView {
 public:
  App();

  // IFrameworkView methods.
  virtual void Initialize(Windows::ApplicationModel::Core::CoreApplicationView ^ applicationView);
  virtual void SetWindow(Windows::UI::Core::CoreWindow ^ window);
  virtual void Load(Platform::String ^ entryPoint);
  virtual void Run();
  virtual void Uninitialize();

 protected:
  // Application lifecycle event handlers.
  void OnActivated(Windows::ApplicationModel::Core::CoreApplicationView ^ applicationView,
                   Windows::ApplicationModel::Activation::IActivatedEventArgs ^ args);
  void OnSuspending(Platform::Object ^ sender, Windows::ApplicationModel::SuspendingEventArgs ^ args);
  void OnResuming(Platform::Object ^ sender, Platform::Object ^ args);

  // Window event handlers.
  void OnWindowSizeChanged(Windows::UI::Core::CoreWindow ^ sender,
                           Windows::UI::Core::WindowSizeChangedEventArgs ^ args);
  void OnVisibilityChanged(Windows::UI::Core::CoreWindow ^ sender,
                           Windows::UI::Core::VisibilityChangedEventArgs ^ args);
  void OnWindowClosed(Windows::UI::Core::CoreWindow ^ sender, Windows::UI::Core::CoreWindowEventArgs ^ args);

  // DisplayInformation event handlers.
  void OnDpiChanged(Windows::Graphics::Display::DisplayInformation ^ sender, Platform::Object ^ args);
  void OnOrientationChanged(Windows::Graphics::Display::DisplayInformation ^ sender, Platform::Object ^ args);
  void OnDisplayContentsInvalidated(Windows::Graphics::Display::DisplayInformation ^ sender, Platform::Object ^ args);

 private:
  std::unique_ptr<PlayerWindow> m_main;
  bool m_windowClosed;
  bool m_windowVisible;
  int m_decoder_state = 0;
};
}  // namespace uwp_dx12_player

ref class Direct3DApplicationSource sealed : Windows::ApplicationModel::Core::IFrameworkViewSource {
 public:
  virtual Windows::ApplicationModel::Core::IFrameworkView ^ CreateView();
};
