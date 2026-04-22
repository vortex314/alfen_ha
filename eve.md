Read this : https://community.home-assistant.io/t/alfen-eve-pro-ev-chargepoint-interface-via-tcp-modbus/346000/66 , give me a good approach distilled from this

The discussion in that specific thread highlights a common "trap" for Alfen users: the charger is extremely strict about the order and timing of Modbus writes.

If you are stuck at 11A and Register 1214 is 0, here is the distilled approach to "taking over" the charger using the logic from the Home Assistant community:

1. The "Heartbeat" Rule (Critical)
The Alfen will not accept a charging command (Register 1210) unless it knows how long to trust that command.

The Approach: You must write to Register 1210 (Max Current) and Register 1211 (Safe Time/Validity) in the same transmission or immediately sequentially.

The Logic: If you send a current limit but the "Validity Time" is 0, the charger considers the command expired immediately and defaults back to its internal safe limit (your 11A ghost).

Recommended Value for 1211: 60 to 120 seconds. Your Home Assistant automation should "ping" the charger with a new value every 30-45 seconds to keep the timer from hitting zero.

2. The Slave Address 200 vs. 1
The thread clarifies a common confusion regarding Slave IDs:

Address 1 or 2: Used for Socket-specific data (like energy used by a specific plug).

Address 200: This is the Management Address. You must use Slave ID 200 when writing to the 1200-series registers to control the station's load balancing logic. If you write to ID 1, it may read successfully but won't flip the "EMS Active" flag (1214).

3. Handle the "Float32" Properly
Home Assistant’s native Modbus integration sends integers by default, but the Alfen expects 32-bit Floats (Big Endian).

The Distilled Code: Don't just send 16. You need to use the data_type: float32 or count: 2 with a payload structure.

Forum Pro-tip: If your values are jumping or being rejected, it's often because the two 16-bit registers that make up the 32-bit float are being received in the wrong order. Use structure: ">f" in your HA configuration.

4. The "0A" vs "6A" behavior
Pause Charging: Setting Register 1210 to 0.0 will pause the session (STATE_B1/B2).

Resume Charging: You must jump straight back to at least 6.0 (the EV standard minimum). If you increment slowly from 0.1, 0.2, etc., the car may time out and "handshake fail" before you hit the 6A threshold.

5. Final "Master" Settings Sequence
To get Register 1214 to show 1, perform this exact sequence in your HA Script:

Write 6.0 (or your fallback) to Register 2076 (Safe Current).

Write 120 to Register 1211 (Validity Time).

Write your desired Amps (e.g., 16.0) to Register 1210.

Wait 2 seconds and read 1214. It should now be 1.

Summary of the approach: Treat the Alfen like it has a "Dead Man's Switch." You must constantly (every minute) tell it: "I am still here, keep charging at X Amps for the next 120 seconds." If you stop talking, it reverts to the 11A limit you are seeing.





