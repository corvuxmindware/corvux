#include "bindings/bindings.h"

#import <Foundation/Foundation.h>
#import <UIKit/UIKit.h>
#import <WebKit/WebKit.h>
#import <objc/runtime.h>

static id corvuxNoInputAccessoryView(id self, SEL _cmd) {
	return nil;
}

static void corvuxPatchWebKitContentView(UIView *view) {
	NSString *className = NSStringFromClass(object_getClass(view));
	if (![className hasPrefix:@"WKContent"] || [className containsString:@"CorvuxNoAccessory"]) {
		return;
	}

	UITextInputAssistantItem *assistant = view.inputAssistantItem;
	assistant.leadingBarButtonGroups = @[];
	assistant.trailingBarButtonGroups = @[];

	Class originalClass = object_getClass(view);
	NSString *patchedClassName = [NSString stringWithFormat:@"%@_CorvuxNoAccessory", NSStringFromClass(originalClass)];
	Class patchedClass = NSClassFromString(patchedClassName);
	if (patchedClass == Nil) {
		patchedClass = objc_allocateClassPair(originalClass, patchedClassName.UTF8String, 0);
		if (patchedClass == Nil) {
			return;
		}

		Method method = class_getInstanceMethod([UIView class], @selector(inputAccessoryView));
		const char *typeEncoding = method ? method_getTypeEncoding(method) : "@@:";
		class_addMethod(
			patchedClass,
			@selector(inputAccessoryView),
			(IMP)corvuxNoInputAccessoryView,
			typeEncoding
		);
		objc_registerClassPair(patchedClass);
	}

	object_setClass(view, patchedClass);
	[view reloadInputViews];
}

static void corvuxPatchWebKitSubviews(UIView *view) {
	corvuxPatchWebKitContentView(view);
	for (UIView *subview in view.subviews) {
		corvuxPatchWebKitSubviews(subview);
	}
}

static void corvuxPatchWebViewKeyboardAccessory(WKWebView *webView) {
	corvuxPatchWebKitSubviews(webView.scrollView);
}

static void (*corvuxOriginalWKWebViewDidMoveToWindow)(id self, SEL _cmd) = NULL;

static void corvuxWKWebViewDidMoveToWindow(id self, SEL _cmd) {
	if (corvuxOriginalWKWebViewDidMoveToWindow != NULL) {
		corvuxOriginalWKWebViewDidMoveToWindow(self, _cmd);
	}

	WKWebView *webView = (WKWebView *)self;
	corvuxPatchWebViewKeyboardAccessory(webView);
	dispatch_async(dispatch_get_main_queue(), ^{
		corvuxPatchWebViewKeyboardAccessory(webView);
	});
}

static void disableWebViewKeyboardAccessoryBar() {
#if TARGET_OS_IOS
	// WKWebView exposes regular HTML inputs through an internal WKContentView,
	// which iOS gives a form-navigation accessory bar above the keyboard.
	// Patch after the WebView has entered the window because WKContentView is
	// created lazily; doing this only at process start can miss the class.
	Method method = class_getInstanceMethod([WKWebView class], @selector(didMoveToWindow));
	if (method == NULL) {
		return;
	}

	corvuxOriginalWKWebViewDidMoveToWindow = (void (*)(id, SEL))method_getImplementation(method);
	method_setImplementation(method, (IMP)corvuxWKWebViewDidMoveToWindow);
#endif
}

int main(int argc, char * argv[]) {
	disableWebViewKeyboardAccessoryBar();
	ffi::start_app();
	return 0;
}
