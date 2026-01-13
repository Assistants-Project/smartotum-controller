# Smartotum Controller

**Smartotum Controller** is a component developed within the **Assistants** project.
It acts as a bridge between Assistants-based smart devices and the Smartotum ecosystem, handling
device discovery, event collection and state synchronization through the Distributed Hash Table (DHT).

The controller is designed to run on the Smartotum gateway and operates entirely on the local network,
without relying on external services.
It follows a **passive controller model**, in which the controller periodically discovers devices on the local network and handles device-generated events to update their descriptions and state within the Smartotum DHT.
