# BAVY OS JavaScript Runtime

BAVY OS includes a JavaScript runtime with ES6-style module imports for
accessing operating system functionality.

---

## Import Syntax

### Namespace Import
Import all module functions under a namespace:

```javascript
import * as fs from "os:fs"
import * as net from "os:net"
import * as sys from "os:sys"
import * as mem from "os:mem"

// Use as: fs.ls(), net.ip(), sys.time(), mem.total()
```

### Named Import
Import specific functions (functions become global):

```javascript
import { ls, read, write } from "os:fs"
import { ip, mac } from "os:net"

// Use directly: ls(), read("/path"), ip()
```

---

## os:fs — Filesystem Module

```javascript
import * as fs from "os:fs"
```

### `fs.ls()`
List all files in the filesystem.

**Returns:** `Array` of objects with:
- `name` (String) - Full path
- `size` (Integer) - Size in bytes
- `is_dir` (Boolean) - Directory flag

```javascript
import * as fs from "os:fs"
let files = fs.ls();
for (let f of files) {
    print(f.name + " - " + f.size + " bytes");
}
```

---

### `fs.read(path)`
Read file contents.

**Parameters:**
- `path` (String) - File path

**Returns:** `String` - File contents, or empty string if not found

```javascript
import * as fs from "os:fs"
let content = fs.read("/home/README.md");
print(content);
```

---

### `fs.write(path, content)`
Write content to a file.

**Parameters:**
- `path` (String) - File path
- `content` (String) - Content to write

**Returns:** `Boolean` - true if successful

```javascript
import * as fs from "os:fs"
if (fs.write("/home/notes.txt", "Hello!")) {
    print("Saved!");
}
```

---

### `fs.exists(path)`
Check if a file exists.

**Parameters:**
- `path` (String) - File path

**Returns:** `Boolean`

```javascript
import * as fs from "os:fs"
if (fs.exists("/home/config.txt")) {
    let cfg = fs.read("/home/config.txt");
}
```

---

### `fs.available()`
Check if filesystem is mounted.

**Returns:** `Boolean`

```javascript
import * as fs from "os:fs"
if (!fs.available()) {
    print("No filesystem!");
}
```

---

## os:net — Network Module

```javascript
import * as net from "os:net"
```

### `net.ip()`
Get the system's IP address.

**Returns:** `String` - IPv4 address (e.g., "10.0.2.15")

```javascript
import * as net from "os:net"
print("IP: " + net.ip());
```

---

### `net.mac()`
Get the MAC address.

**Returns:** `String` - MAC address (e.g., "52:54:00:12:34:56")

```javascript
import * as net from "os:net"
print("MAC: " + net.mac());
```

---

### `net.gateway()`
Get the default gateway.

**Returns:** `String` - Gateway IP address

```javascript
import * as net from "os:net"
print("Gateway: " + net.gateway());
```

---

### `net.dns()`
Get the DNS server address.

**Returns:** `String` - DNS server IP

```javascript
import * as net from "os:net"
print("DNS: " + net.dns());
```

---

### `net.prefix()`
Get the network prefix length.

**Returns:** `Integer` - Prefix length (e.g., 24 for /24)

```javascript
import * as net from "os:net"
print("Prefix: /" + net.prefix());
```

---

### `net.available()`
Check if network is initialized.

**Returns:** `Boolean`

```javascript
import * as net from "os:net"
if (net.available()) {
    print("Network is up!");
}
```

---

## os:sys — System Module

```javascript
import * as sys from "os:sys"
```

### `sys.time()`
Get milliseconds since boot.

**Returns:** `Integer` - Uptime in milliseconds

```javascript
import * as sys from "os:sys"
print("Uptime: " + (sys.time() / 1000) + " seconds");
```

---

### `sys.sleep(ms)`
Sleep for specified milliseconds.

**Parameters:**
- `ms` (Integer) - Milliseconds to sleep

```javascript
import * as sys from "os:sys"
print("Waiting...");
sys.sleep(1000);  // Sleep 1 second
print("Done!");
```

---

### `sys.cwd()`
Get current working directory.

**Returns:** `String` - Current directory path

```javascript
import * as sys from "os:sys"
print("Current dir: " + sys.cwd());
```

---

### `sys.version()`
Get kernel version string.

**Returns:** `String` - Version (e.g., "BAVY OS")

```javascript
import * as sys from "os:sys"
print(sys.version());
```

---

### `sys.arch()`
Get CPU architecture.

**Returns:** `String` - Architecture (e.g., "RISC-V 64-bit (RV64GC)")

```javascript
import * as sys from "os:sys"
print("Arch: " + sys.arch());
```

---

## os:mem — Memory Module

```javascript
import * as mem from "os:mem"
```

### `mem.total()`
Get total heap size.

**Returns:** `Integer` - Total bytes

```javascript
import * as mem from "os:mem"
print("Total: " + (mem.total() / 1024) + " KB");
```

---

### `mem.used()`
Get used heap memory.

**Returns:** `Integer` - Used bytes

```javascript
import * as mem from "os:mem"
print("Used: " + (mem.used() / 1024) + " KB");
```

---

### `mem.free()`
Get free heap memory.

**Returns:** `Integer` - Free bytes

```javascript
import * as mem from "os:mem"
print("Free: " + (mem.free() / 1024) + " KB");
```

---

### `mem.stats()`
Get memory statistics.

**Returns:** `Object` with:
- `used` (Integer) - Used bytes
- `free` (Integer) - Free bytes

```javascript
import * as mem from "os:mem"
let s = mem.stats();
print("Used: " + s.used + ", Free: " + s.free);
```

---

## Global Functions

These functions are always available without imports.

### Output

| Function | Description |
|----------|-------------|
| `print(value)` | Print with newline |
| `write(value)` | Print without newline |
| `debug(value)` | Debug output with type info |

### Parsing

| Function | Description |
|----------|-------------|
| `parse_int(str)` | Parse string to integer |
| `parse_float(str)` | Parse string to float |

### Type Checking

| Function | Description |
|----------|-------------|
| `type_of(value)` | Get type name |
| `is_string(value)` | Check if string |
| `is_int(value)` | Check if integer |
| `is_float(value)` | Check if float |
| `is_array(value)` | Check if array |

### String Utilities

| Function | Description |
|----------|-------------|
| `repeat(str, n)` | Repeat string n times |
| `pad_left(str, width, char)` | Left-pad string |
| `pad_right(str, width, char)` | Right-pad string |
| `join(array, sep)` | Join array with separator |

### Iteration

| Function | Description |
|----------|-------------|
| `range(end)` | Array [0, end) |
| `range(start, end)` | Array [start, end) |
| `range(start, end, step)` | Array with step |

---

## Data Types

| Type | Examples |
|------|----------|
| Integer | `42`, `-10`, `0` |
| Float | `3.14`, `-0.5` |
| String | `"hello"`, `'world'` |
| Boolean | `true`, `false` |
| Array | `[1, 2, 3]`, `[]` |
| Object | `#{key: "value"}` |

---

## Control Flow

### If/Else
```javascript
if condition {
    // ...
} else if other {
    // ...
} else {
    // ...
}
```

### For Loop
```javascript
for item in array {
    print(item);
}

for i in range(10) {
    print(i);
}
```

### While Loop
```javascript
let i = 0;
while i < 10 {
    print(i);
    i += 1;
}
```

---

## Functions

```javascript
fn greet(name) {
    print("Hello, " + name + "!");
}

fn add(a, b) {
    return a + b;
}

greet("World");
let sum = add(2, 3);
```

---

## Special Variable

### `ARGS`
Array of command-line arguments passed to the script.

```javascript
// If called as: myscript arg1 arg2
// ARGS = ["arg1", "arg2"]

for arg in ARGS {
    print("Argument: " + arg);
}
```

---

## Example Script

```javascript
// sysinfo.js - Display system information

import * as fs from "os:fs"
import * as net from "os:net"
import * as sys from "os:sys"
import * as mem from "os:mem"

print("=== System Info ===");
print("Kernel:  " + sys.version());
print("Arch:    " + sys.arch());
print("Uptime:  " + (sys.time() / 1000) + "s");

print("");
print("=== Memory ===");
let m = mem.stats();
print("Used: " + (m.used / 1024) + " KB");
print("Free: " + (m.free / 1024) + " KB");

print("");
print("=== Network ===");
if net.available() {
    print("IP:      " + net.ip());
    print("Gateway: " + net.gateway());
} else {
    print("Network offline");
}

print("");
print("=== Filesystem ===");
if fs.available() {
    let files = fs.ls();
    print("Files: " + files.len());
}
```
