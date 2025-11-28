const fs = require('fs');
const path = require('path');

const projects = ['kernel', 'relay', 'riscv-vm'];

console.log('Syncing Cargo.toml versions from package.json...');

projects.forEach(project => {
  const pkgPath = path.join(process.cwd(), project, 'package.json');
  const cargoPath = path.join(process.cwd(), project, 'Cargo.toml');

  if (fs.existsSync(pkgPath) && fs.existsSync(cargoPath)) {
    try {
      const pkg = JSON.parse(fs.readFileSync(pkgPath, 'utf8'));
      const version = pkg.version;
      
      let cargo = fs.readFileSync(cargoPath, 'utf8');
      // Match version = "x.y.z" in the [package] section
      const versionRegex = /^version\s*=\s*".*"/m;
      
      if (versionRegex.test(cargo)) {
        const newCargo = cargo.replace(versionRegex, `version = "${version}"`);
        if (cargo !== newCargo) {
          fs.writeFileSync(cargoPath, newCargo);
          console.log(`✅ Updated ${project}/Cargo.toml to version ${version}`);
        } else {
          console.log(`ℹ️  ${project}/Cargo.toml already at ${version}`);
        }
      } else {
        console.warn(`⚠️  Could not find version key in ${project}/Cargo.toml`);
      }
    } catch (err) {
      console.error(`❌ Error processing ${project}: ${err.message}`);
      process.exit(1);
    }
  } else {
    console.log(`Skipping ${project}: missing package.json or Cargo.toml`);
  }
});

