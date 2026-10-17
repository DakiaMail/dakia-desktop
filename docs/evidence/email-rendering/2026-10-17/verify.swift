import Cocoa
import WebKit

let output = CommandLine.arguments[1]
let baseURL = CommandLine.arguments[2]
let app = NSApplication.shared
app.setActivationPolicy(.regular)
final class Runner: NSObject, WKScriptMessageHandler, WKNavigationDelegate {
 var window: NSWindow!
 var web: WKWebView!
 var finished = false
 func start() {
  let config = WKWebViewConfiguration()
  config.userContentController.add(self, name: "verification")
  config.preferences.javaScriptCanOpenWindowsAutomatically = false
  window = NSWindow(contentRect: NSRect(x:0,y:0,width:1280,height:1800), styleMask:[.titled,.closable,.resizable], backing:.buffered, defer:false)
  window.title = "Dakia Receipt Native WebKit Verification"
  web = WKWebView(frame: NSRect(x:0,y:0,width:1280,height:1800), configuration:config)
  web.navigationDelegate = self
  window.contentView = web
  window.makeKeyAndOrderFront(nil)
  app.activate(ignoringOtherApps:true)
  web.load(URLRequest(url:URL(string:baseURL)!))
  DispatchQueue.main.asyncAfter(deadline:.now()+90) { if !self.finished { self.finish(["pass":false,"error":"native harness timed out","url":self.web.url?.absoluteString ?? "none"]) } }
 }
 func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error:Error) { finish(["pass":false,"error":error.localizedDescription]) }
 func userContentController(_ userContentController:WKUserContentController, didReceive message:WKScriptMessage) {
  guard let body = message.body as? [String:Any], let kind=body["kind"] as? String else {return}
  if kind == "click" {
   let x=(body["x"] as! NSNumber).doubleValue, y=(body["y"] as! NSNumber).doubleValue
   let point=NSPoint(x:x,y:web.bounds.height-y)
   for type in [NSEvent.EventType.leftMouseDown,.leftMouseUp] {
    if let event=NSEvent.mouseEvent(with:type,location:point,modifierFlags:[],timestamp:ProcessInfo.processInfo.systemUptime,windowNumber:window.windowNumber,context:nil,eventNumber:1,clickCount:1,pressure:type == .leftMouseDown ? 1:0) { window.sendEvent(event) }
   }
   DispatchQueue.main.asyncAfter(deadline:.now()+0.1) { self.web.evaluateJavaScript("window.nativeAck()",completionHandler:nil) }
  } else if kind == "capture", let name=body["name"] as? String {
   let config=WKSnapshotConfiguration()
   config.rect=web.bounds
   web.takeSnapshot(with:config) { image,error in
    if let image=image, let tiff=image.tiffRepresentation, let bitmap=NSBitmapImageRep(data:tiff), let png=bitmap.representation(using:.png,properties:[:]) {
     try? png.write(to:URL(fileURLWithPath:output).appendingPathComponent(name+".png"))
    } else { print("snapshot failed: \(String(describing:error))") }
    self.web.evaluateJavaScript("window.nativeAck()",completionHandler:nil)
   }
  } else if kind == "result" { var result=body; result["nativeWebviewURL"]=web.url?.absoluteString;result["executable"]=Bundle.main.executablePath;result["pid"]=ProcessInfo.processInfo.processIdentifier;result["macOS"]=ProcessInfo.processInfo.operatingSystemVersionString; finish(result) }
 }
 func finish(_ result:[String:Any]) {
  guard !finished else{return};finished=true
  if let data=try? JSONSerialization.data(withJSONObject:result,options:[.prettyPrinted,.sortedKeys]) {try? data.write(to:URL(fileURLWithPath:output).appendingPathComponent("result.json")); print(String(data:data,encoding:.utf8)!)}
  app.stop(nil)
  let event=NSEvent.otherEvent(with:.applicationDefined,location:.zero,modifierFlags:[],timestamp:0,windowNumber:0,context:nil,subtype:0,data1:0,data2:0)!
  app.postEvent(event,atStart:true)
 }
}
let runner=Runner();runner.start();app.run()
