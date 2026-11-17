
import socket
import struct
import sys

PI_IP = "169.254.16.1"
DOIP_PORT = 13400

TESTER_ADDRESS = 0x0E00
ECU_ADDRESS = 0x1000

def receive_exact(sock, count):
    data = b""
    while len(data) < count:
        chunk = sock.recv(count - len(data))
        if not chunk:
            raise ConnectionError("Connection closed by ECU")
        data += chunk
    return data

def send_doip(sock, payload_type, payload):
    header = struct.pack(
        ">BBHI",
        0x02, 0xFD, payload_type, len(payload)
    )
    sock.sendall(header + payload)

def receive_doip(sock):
    header = receive_exact(sock, 8)
    version, inverse, payload_type, length = struct.unpack(
        ">BBHI", header
    )

    if version ^ inverse != 0xFF:
        raise ValueError("Invalid DoIP header")

    payload = receive_exact(sock, length)
    return payload_type, payload

def test_uds(request_hex):
    uds_request = bytes.fromhex(request_hex)
    print(f"UDS bytes sent ({len(uds_request)}): {uds_request.hex(' ').upper()}")

    with socket.create_connection((PI_IP, DOIP_PORT), timeout=5) as sock:
        sock.settimeout(5)

        # DoIP routing activation
        activation = struct.pack(">HB", TESTER_ADDRESS, 0x00) + bytes(4)
        send_doip(sock, 0x0005, activation)

        while True:
            msg_type, payload = receive_doip(sock)

            if msg_type == 0x0006:
                if len(payload) < 5 or payload[4] not in (0x10, 0x11):
                    raise RuntimeError(
                        f"Routing activation rejected: {payload.hex(' ')}"
                    )
                print("Routing activation successful")
                break

        # Send diagnostic message
        diagnostic_payload = struct.pack(
            ">HH", TESTER_ADDRESS, ECU_ADDRESS
        ) + uds_request

        send_doip(sock, 0x8001, diagnostic_payload)

        print("Request: ", uds_request.hex(" ").upper())

        while True:
            msg_type, payload = receive_doip(sock)

            if msg_type == 0x8001:
                if len(payload) < 4:
                    raise ValueError("Short diagnostic response")

                source, target = struct.unpack(">HH", payload[:4])
                if source != ECU_ADDRESS or target != TESTER_ADDRESS:
                    continue

                response = payload[4:]
                print("Response:", response.hex(" ").upper())
                return

            if msg_type == 0x8003:
                raise RuntimeError(
                    f"Diagnostic negative ACK: {payload.hex(' ')}"
                )

if __name__ == "__main__":
    if len(sys.argv) != 2:
        print('Usage: python3 test_dtc_doip.py "19 02 FF"')
        sys.exit(1)

    try:
        test_uds(sys.argv[1])
    except (OSError, ValueError, RuntimeError) as error:
        print(f"Error: {error}")
        sys.exit(1)
