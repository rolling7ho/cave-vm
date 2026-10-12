// P2.8 vmnet check (ROADMAP.md A6): tries to start a shared-mode (NAT)
// vmnet interface as the current user and prints the status.
import Foundation
import vmnet

let desc = xpc_dictionary_create(nil, nil, 0)
xpc_dictionary_set_uint64(desc, vmnet_operation_mode_key, UInt64(operating_modes_t.VMNET_SHARED_MODE.rawValue))
let queue = DispatchQueue(label: "vmnet-probe")
let done = DispatchSemaphore(value: 0)
var result: vmnet_return_t = .VMNET_FAILURE
let iface = vmnet_start_interface(desc, queue) { status, _ in
    result = status
    done.signal()
}
if iface == nil {
    print("vmnet-start: FAILED (no interface returned)")
    exit(2)
}
_ = done.wait(timeout: .now() + 10)
print("vmnet-start: status \(result.rawValue) (\(result == .VMNET_SUCCESS ? "ok" : "failed"))")
if result == .VMNET_SUCCESS {
    _ = vmnet_stop_interface(iface!, queue) { _ in done.signal() }
    _ = done.wait(timeout: .now() + 5)
    exit(0)
}
exit(3)
