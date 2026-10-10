/* ssh_bridge.h
 *
 * SSH-to-serial bridge for ESP32-S3.
 *
 * The bridge listens for SSH connections on a TCP port.  Each session
 * presents a terminal whose I/O is directly coupled to the COM1 UART
 * (u-blox GPS) serial device.
 *
 * Thread safety:
 *   - ssh_bridge_init()     must be called once before tasks start.
 *   - ssh_bridge_serial_rx() may be called from any task/ISR context.
 */

#pragma once
#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/**
 * Callback invoked when the SSH client sends data that should be forwarded
 * to the serial device (UART TX).
 *
 * @param data  pointer to the bytes received from SSH client
 * @param len   number of bytes
 */
typedef void (*ssh_serial_tx_cb_t)(const uint8_t *data, size_t len);

/**
 * Initialise and start the SSH bridge.
 *
 * Spawns two FreeRTOS tasks:
 *   - SSH server listener (TCP accept loop)
 *   - SSH session handler (SSH protocol + data forwarding)
 *
 * @param port        TCP port to listen on (typically 22)
 * @param username    accepted username (currently informational; any name is accepted)
 * @param password    accepted password (NULL falls back to compile-time default)
 */
int ssh_bridge_init(uint16_t port,
                    const char *username, const char *password);

/**
 * Feed data received from the serial device (UART RX) into the SSH
 * bridge.  The data is buffered and sent to any currently connected SSH
 * client as terminal output.
 *
 * Safe to call from an interrupt or any FreeRTOS task.
 *
 * @param data  received bytes
 * @param len   number of bytes
 */
void ssh_bridge_serial_rx(const uint8_t *data, size_t len);

#ifdef __cplusplus
}
#endif
