const fs = require('fs');

const input = "● The design document is complete.";
console.log("Raw string length:", input.length);
console.log("First char:", input.charCodeAt(0).toString(16));

const serialized = JSON.stringify(input);
console.log("Serialized:", serialized);

const deserialized = JSON.parse(serialized);
console.log("Deserialized:", deserialized);
