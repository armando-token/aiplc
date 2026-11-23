# Inventario de Hardware y Sistema — Luckfox Lyra PLC

**Fecha de verificación en unidad:** 2026-11-22 (reloj local del PLC sin sincronizar en prueba USB offline: `2026-04-27`)  
**Acceso verificado:** USB-C ADB (`serial: f0fe1f680160dbd9`, túnel `tcp:2223 -> tcp:22`, alias `ssh luckfox-usb`, usuario `root`)  
**Estado:** Medido directamente sobre la unidad física.

---

## 1. Identidad de placa y SoC

| Parámetro | Valor observado en la unidad |
| --- | --- |
| Modelo Device Tree (`/proc/device-tree/model`) | `Luckfox Lyra PLC` |
| Compatible (`/proc/device-tree/compatible`) | `rockchip,rk3506g-demo-display-control`, `rockchip,rk3506` |
| Serial SoC (`/proc/device-tree/serial-number`) | `95b7c21fd4859196` |
| Arquitectura / CPU | `armv7l` (`armhf`), 3× ARM Cortex-A7 (`cpu0..2`, 600–1512 MHz, `ondemand`) |
| Flags ISA | `half thumb fastmult vfp edsp neon vfpv3 tls vfpv4 idiva idivt vfpd32 lpae` |
| RAM utilizable (`free -h`) | `464 MiB` total (`~360 MiB` disponibles tras arranque base) + `255 MiB` swap |
| Almacenamiento eMMC (`/dev/mmcblk0`, `7.3G`) | `p1`: 4M, `p2`: 1M, `p3`: 16M, `p4`: 12M, `p5`: 2.0G (`/userdata`, 15M usados), `p6`: 5.2G (`/`, 1.4G usados) |
| Temperatura SoC inicial (`thermal_zone0/temp`) | `46386` m°C (`46.39 °C`) |

> **Nota sobre Cortex-M0 / NPU:** La documentación de Luckfox/RK3506 menciona núcleos auxiliares en el silicio, pero en el kernel y rootfs inspeccionados no hay firmware ni dispositivo remoto activo para delegar control en tiempo real al M0; la ejecución en esta unidad corre en Linux ARMv7 (`Cortex-A7`).

---

## 2. Sistema Operativo, Kernel y Capacidades de Aislamiento

| Componente | Valor observado |
| --- | --- |
| Distribución | `Debian GNU/Linux 12 (bookworm)` `armhf` |
| C Library (`ldd --version`) | `Debian GLIBC 2.36-9+deb12u14` |
| Kernel activo (`uname -a`) | `Linux luckfox 6.1.99 #14 SMP PREEMPT Tue Jul 28 14:29:03 CST 2026 armv7l GNU/Linux` |
| Modo RT | **NO activo** en el boot actual (`/sys/kernel/realtime` ausente, kernel `#14 SMP PREEMPT` de fábrica). En `/boot` existe `vmlinuz-6.1.0-51-rt-armmp`, pero `cmdline` arranca la imagen de fábrica Rockchip. |
| Cmdline | `storagemedia=emmc androidboot.mode=normal rootwait earlycon=uart8250,mmio32,0xff0a0000 console=ttyFIQ0 root=/dev/mmcblk0p6 rootfstype=ext4 ...` |
| Compiladores / Runtimes presentes | `gcc 12.2.0`, `g++ 12.2.0`, `make`, `python3 3.11.2`, `node v20.17.0`, `npm 10.8.2`, `libseccomp2 2.5.4`, `libsqlite3-0 3.40.1` |
| Herramientas ausentes | Sin `cargo`, `rustc`, `gpiodetect`/`gpioinfo`, `cyclictest`, `rtla`, ni conectividad WAN directa (el PLC recibe paquetes compilados desde la Mac mini por USB/SSH). |

### Primitivas de aislamiento verificadas en kernel `6.1.99`
- **Disponibles y activas:**
  - `cgroup v2` montado en `/sys/fs/cgroup` con controladores `cpuset`, `cpu`, `memory` (soporta `memory.max`, `memory.current`, `memory.peak`, `memory.events`, `cpu.max`).
  - `Seccomp-BPF` (`PR_GET_SECCOMP == 0`, `CONFIG_SECCOMP=y`).
  - Drop de privilegios a UID/GID `65534` (`nobody`), `PR_SET_NO_NEW_PRIVS`, `setrlimit` (`RLIMIT_NOFILE`, `RLIMIT_FSIZE`, `RLIMIT_CORE`, `RLIMIT_NPROC`), y cierre de descriptores heredados.
- **No disponibles en tiempo de ejecución en `/proc/self/ns/`:**
  - Solo están presentes los namespaces `cgroup` y `mnt`. Aunque `/boot/config-6.1.0-51-rt-armmp` lista `NET_NS`/`PID_NS`/`USER_NS`, el kernel `6.1.99` en ejecución no expone `net`, `pid` ni `user` namespaces ni el controlador `pids` en `cgroup.controllers`.
  - **Consecuencia arquitectónica:** El aislamiento del worker en Lyra combina **Seccomp-BPF** (bloqueo en kernel de `socket`, `connect`, `bind`, `open`/`openat` fuera de memoria/STDIO, `execve`, `fork`/`clone`, `ioctl`), `cgroup v2` (`memory.max`, `cpu.max`), `setrlimit` y UID `nobody`. Si se ejecuta en un kernel que sí exponga `CLONE_NEWNET`/`CLONE_NEWPID`, el sandbox los activa y registra las capacidades efectivas al arrancar.

---

## 3. Mapa de Hardware, Buses, Relés y Pantalla

### 3.1 Chips GPIO (`/dev/gpiochip0` .. `/dev/gpiochip4`)
Verificado vía `GPIO_GET_CHIPINFO_IOCTL` y `GPIO_V2_GET_LINEINFO_IOCTL`:
- `gpiochip0` (`gpio0`, 32 líneas):
  - Línea `0`: `consumer='reset'`, `out hi ACTIVE LOW`
  - Línea `2`: `consumer='dc'`, `out hi`
  - Línea `6`: `consumer='reset'`, `out hi ACTIVE LOW`
  - Línea `15`: libre (`consumer=''`) — configurado en `plc_config.json` como `txden_pin` para UART2 RS-485.
  - **Línea `17`**: libre (`consumer=''`) — **`relay0` (`Relay 0`, `active_low: false`)**.
  - **Línea `18`**: libre (`consumer=''`) — **`relay1` (`Relay 1`, `active_low: false`)**.
- `gpiochip1` (`gpio1`, 32 líneas): `gpio-32` (`work-led`), `gpio-53` (`bt_default_poweron`).
- `gpiochip2..4` (`gpio2..4`, 32 líneas c/u): libres.

### 3.2 Interfaces Serie, CAN, Red, Watchdog y Pantalla
- **RS-485 / RS-422:** `/dev/ttyS2` (uart2, 9600 8N1 por defecto, `txden_pin: 15` en `plc_config.json`).
- **RS-232:** `/dev/ttyS1` (uart1).
- **Debug Console:** `/dev/ttyFIQ0`.
- **CAN:** `can0` (`SocketCAN`, `can-utils` instalado, estado `DOWN`).
- **ADC:** `/sys/bus/iio/devices/iio:device0`.
- **Red:** `eth0` (`NO-CARRIER`, `DOWN`), `eth1` (`100Mbps/Full`, sin IPv4 al momento de inspección porque no hay servidor DHCP en el enlace directo), `wlan0` (`DORMANT`). USB-C configurado únicamente con `ffs.adb` (`usbdevice.service`).
- **Watchdogs:** `/dev/watchdog`, `/dev/watchdog0`, `/dev/watchdog1` (`watchdog.enabled: false` en fábrica).
- **Pantalla LCD:** `/dev/fb0` (`170x320`, `16 bpp` RGB565), controlada por `/userdata/lyraplc/bin/plc_display` a través de `/var/run/kvm_lyra_plc_display.sock`.
  - Strings observados en `plc_display` indican mensajes JSON tipo `ui_set_text` con `{params: {obj: ..., text: ...}}` y widgets como `Main_Eth0_Ipaddr_Label`, `Main_Eth1_Ipaddr_Label`, `Main_Wlan0_Ipaddr_Label`, `Monitor_CPU_Temp_Label`.

---

## 4. Software de Fábrica, Propiedad GPIO y Recuperación

| Artefacto | Ubicación | SHA-256 |
| --- | --- | --- |
| `plc_app` | `/userdata/lyraplc/bin/plc_app` | `db815064a65cb8719a628842525f8894d482cafc3c2130d247ac08108b70bf75` |
| `plc_display` | `/userdata/lyraplc/bin/plc_display` | `f1ba93a18fa7fc53f1bb1d387daf123efbd6b392d696ab361af7b00fba3658b1` |
| `plc_config.json` | `/userdata/plc_config.json` | `f5064d5a6e84a89239b5caae27850e6b618dcc92982b168cb7b88cac5a320080` |

- **Servicio de fábrica:** `lyra-plc.service` (`/etc/systemd/system/lyra-plc.service`) levanta `/userdata/lyraplc/bin/plc_app` en el puerto `:80`. En la configuración actual (`plc_config.json`), `uart1`, `uart2`, `can0`, `poll` y `protocol` están `enabled: false`, y los relés están en `state: false`.
- **Propiedad exclusiva de GPIO:** Aunque `plc_app` no mantiene las líneas 17 y 18 abiertas cuando no hay reglas activas, puede escribirlas si se opera la WebUI o una tarea interna. Para garantizar propietario único en pruebas HIL y operación del supervisor:
  1. Detener temporalmente `systemctl stop lyra-plc.service` (o deshabilitarlo solo durante ventana de banco si se requiere exclusividad tras reboot, conservando siempre los binarios originales intactos).
  2. Reclamar `gpiochip0` líneas `17` y `18` mediante **GPIO chardev v2 (`GPIO_V2_GET_LINE_IOCTL`)** con `consumer = "plc-auto-supervisor"`. Cualquier segundo consumidor recibe `EBUSY` del kernel.
  3. Nuestro supervisor expone su API en el puerto `:8443` (y consola local en `:8090`), dejando `:80` y `/userdata/lyraplc` intactos.
- **Recuperación:** Todos los binarios, perfiles, paquetes y journals de este proyecto se despliegan aislados en `/userdata/plc-auto/`. No se modifican `/boot`, particiones de arranque ni `/userdata/lyraplc`. Para volver al estado de fábrica: detener `plc-auto` y ejecutar `systemctl start lyra-plc.service`.
