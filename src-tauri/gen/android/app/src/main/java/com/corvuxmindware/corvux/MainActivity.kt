package com.corvuxmindware.corvux

import android.graphics.Color
import android.os.Bundle
import android.view.View
import android.webkit.WebView

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    window.setBackgroundDrawable(android.graphics.drawable.ColorDrawable(Color.rgb(26, 26, 36)))
    super.onCreate(savedInstanceState)
  }

  override fun onWebViewCreate(webView: WebView) {
    super.onWebViewCreate(webView)
    webView.setBackgroundColor(Color.rgb(26, 26, 36))
    webView.setLayerType(View.LAYER_TYPE_HARDWARE, null)
  }
}
