const { VersionActions } = require('nx/src/command-line/release/version/version-actions');
const path = require('path');

class RustVersionActions extends VersionActions {
  validManifestFilenames = ['package.json', 'Cargo.toml'];

  async validate(tree) {
    // Override validation to check for either package.json OR Cargo.toml
    const projectRoot = this.projectGraphNode.data.root;
    
    const pkgPath = path.join(projectRoot, 'package.json');
    const cargoPath = path.join(projectRoot, 'Cargo.toml');
    
    const hasPkg = tree.exists(pkgPath);
    const hasCargo = tree.exists(cargoPath);
    
    if (!hasPkg && !hasCargo) {
      throw new Error(
        `The project "${this.projectGraphNode.name}" does not have a package.json or Cargo.toml file available in ${projectRoot}/`
      );
    }
    
    // Update manifestsToUpdate based on what exists
    this.manifestsToUpdate = [];
    if (hasPkg) {
      this.manifestsToUpdate.push({ manifestPath: pkgPath, preserveLocalDependencyProtocols: true });
    }
    if (hasCargo) {
      this.manifestsToUpdate.push({ manifestPath: cargoPath, preserveLocalDependencyProtocols: false });
    }
  }

  async readCurrentVersionFromSourceManifest(tree) {
    const projectRoot = this.projectGraphNode.data.root;

    // Try package.json first
    const pkgPath = path.join(projectRoot, 'package.json');
    if (tree.exists(pkgPath)) {
      try {
        const pkg = JSON.parse(tree.read(pkgPath, 'utf-8'));
        if (pkg.version) {
          return { currentVersion: pkg.version, manifestPath: pkgPath };
        }
      } catch (e) {
        // ignore
      }
    }

    // Try Cargo.toml
    const cargoPath = path.join(projectRoot, 'Cargo.toml');
    if (tree.exists(cargoPath)) {
      try {
        const cargo = tree.read(cargoPath, 'utf-8');
        const match = cargo.match(/^version\s*=\s*"(.*)"/m);
        if (match && match[1]) {
          return { currentVersion: match[1], manifestPath: cargoPath };
        }
      } catch (e) {
        // ignore
      }
    }

    return null;
  }

  async readCurrentVersionFromRegistry(tree, currentVersionResolverMetadata) {
    // Not supporting registry resolution for Rust projects
    return null;
  }

  async readCurrentVersionOfDependency(tree, projectGraph, dependencyProjectName) {
    // For now, return null - we don't track cross-project dependencies for versioning
    return { currentVersion: null, dependencyCollection: null };
  }

  async updateProjectVersion(tree, newVersion) {
    const projectRoot = this.projectGraphNode.data.root;
    const logs = [];

    // 1. Update package.json if it exists
    const pkgPath = path.join(projectRoot, 'package.json');
    if (tree.exists(pkgPath)) {
      const pkg = JSON.parse(tree.read(pkgPath, 'utf-8'));
      pkg.version = newVersion;
      tree.write(pkgPath, JSON.stringify(pkg, null, 2) + '\n');
      logs.push(`Updated ${pkgPath} to ${newVersion}`);
    }

    // 2. Update Cargo.toml if it exists
    const cargoPath = path.join(projectRoot, 'Cargo.toml');
    if (tree.exists(cargoPath)) {
      let cargo = tree.read(cargoPath, 'utf-8');
      const versionRegex = /^version\s*=\s*".*"/m;
      if (versionRegex.test(cargo)) {
        cargo = cargo.replace(versionRegex, `version = "${newVersion}"`);
        tree.write(cargoPath, cargo);
        logs.push(`Updated ${cargoPath} to ${newVersion}`);
      }
    }

    return logs;
  }

  async updateProjectDependencies(tree, projectGraph, dependenciesToUpdate) {
    // For now, not updating dependencies across projects
    return [];
  }
}

module.exports = RustVersionActions;
