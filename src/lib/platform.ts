/** The same app runs on Windows and Linux; a few labels name things only
 * one of them has (File Explorer, Credential Manager, the snipping bar).
 * WebView2 and WebKitGTK both say which system they run on. */
export const isLinux = typeof navigator !== "undefined" && /Linux/.test(navigator.userAgent) && !/Android/.test(navigator.userAgent);
