import CoreGraphics

let list = CGWindowListCopyWindowInfo(.optionAll, kCGNullWindowID) as! [[String: Any]]
let count = list.filter {
    guard let name = $0[kCGWindowName as String] as? String else { return false }
    return name.hasPrefix("Packages Display ") && name.hasSuffix(" Shield")
}.count
print("Packages shield windows: \(count)  (total \(list.count))")
