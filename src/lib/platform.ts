/** The OS the webview runs on; picks how the window frame is drawn. */
const ua = navigator.userAgent

export const isWindows = ua.includes('Windows')
export const isMac = !isWindows && ua.includes('Mac')
